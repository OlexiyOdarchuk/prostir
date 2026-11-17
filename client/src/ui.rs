//! The floating menu panel. It is drawn on the CPU into an RGBA image and only
//! re-uploaded when something changes (hover, a click, news from the host).

use std::collections::HashMap;

use fontdue::{Font, FontSettings, Metrics};
use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Transform};

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
    pub tab: Tab,
    pub lang: Lang,
    /// First run: show the tips page instead of the tabs.
    pub intro: bool,
    pub profiles: Vec<Profile>,
    /// The profile last switched to (until something is changed by hand).
    pub active_profile: Option<usize>,
}

/// Pages of the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Monitors,
    Profiles,
    Look,
    Sound,
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
    Close,
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Normal,
    Selected,
    Danger,
    /// Asking to confirm a destructive action.
    Armed,
    Disabled,
}

enum Item {
    Panel(Rect),
    Box(Rect, [u8; 4]),
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
const MUTED: [u8; 4] = [150, 156, 170, 255];
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
                    boxes.push((*r, label.clone(), true));
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
        let status = match &s.host {
            _ if s.pairing.is_some() => l.t("Сполучення…", "Pairing…").to_owned(),
            Some(h) => format!("{} {h}", l.t("Підключено до", "Connected to")),
            None => l.t("Шукаю ПК…", "Looking for a PC…").to_owned(),
        };
        let sw = self.text_width(&status, 21.0, false);
        let change_w = if s.host.is_some() { 190.0 } else { 0.0 };
        items.push(text(
            w - 120.0 - change_w - sw,
            66.0,
            21.0,
            false,
            MUTED,
            &status,
        ));
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
        if let Some(p) = &s.pairing {
            self.build_pairing(l, p, &mut items, lx, lw, 150.0);
            return items;
        }
        if s.intro && s.host.is_some() {
            build_intro(l, &mut items, lx, w - 2.0 * lx);
            return items;
        }

        // Tabs.
        let tabs = [
            (Tab::Monitors, l.t("Монітори", "Monitors")),
            (Tab::Profiles, l.t("Профілі", "Profiles")),
            (Tab::Look, l.t("Вигляд", "Look")),
            (Tab::Sound, l.t("Звук і мова", "Sound, language")),
        ];
        for (i, (tab, label)) in tabs.into_iter().enumerate() {
            let r = rect(lx + i as f32 * 210.0, 128.0, 200.0, 50.0);
            if s.tab == tab {
                items.push(button(r, label, Style::Selected, None));
            } else {
                items.push(button(r, label, Style::Normal, Some(Action::Tab(tab))));
            }
        }

        let top = 230.0;
        match s.tab {
            Tab::Monitors => {
                if s.host.is_none() {
                    self.build_connection(s, &mut items, lx, lw, top);
                } else {
                    let y = monitor_list(s, &mut items, lx, lw, top - 12.0);
                    hints(l, &mut items, lx, y.max(640.0) + 30.0);
                }
                build_monitor_options(s, &mut items, rx, rw, top);
            }
            Tab::Profiles => {
                build_profiles(s, &mut items, lx, rx + rw - lx, top - 12.0);
            }
            Tab::Look => {
                build_look(s, &mut items, lx, lw, rx, rw, top);
            }
            Tab::Sound => {
                build_sound(s, &mut items, lx, lw, top - 12.0);
                let mut y = top;
                segmented(
                    &mut items,
                    rx,
                    rw,
                    &mut y,
                    l.t("Мова · Language", "Language · Мова"),
                    vec![
                        (
                            l.t("Як на ПК", "Like the PC"),
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
        items.push(text(
            lx,
            y,
            21.0,
            false,
            MUTED,
            l.t("Підключення до ПК", "Connect to a PC"),
        ));
        y += 40.0;
        for line in [
            l.t(
                "Запустіть Prostir на ноутбуці в тій самій мережі Wi‑Fi",
                "Start Prostir on the PC on the same Wi‑Fi network",
            ),
            l.t(
                "або підключіть шолом кабелем.",
                "or connect the headset with a cable.",
            ),
        ] {
            items.push(text(lx, y, 19.0, false, TEXT, line));
            y += 28.0;
        }
        y += 12.0;
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
                        (Style::Armed, _) => ([220, 70, 70, 255], [255, 255, 255, 255]),
                        (Style::Disabled, _) => ([255, 255, 255, 8], [110, 115, 128, 255]),
                        (Style::Danger, true) => ([220, 70, 70, 255], [255, 255, 255, 255]),
                        (_, true) => ([255, 255, 255, 46], [255, 255, 255, 255]),
                        _ => ([255, 255, 255, 20], TEXT),
                    };
                    fill(&mut pixmap, rounded(*r, 12.0), bg);
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
    // Physical display mirror.
    let note = match (s.mirror_wanted, s.mirror_active) {
        (true, true) => l.t("показано", "shown"),
        (true, false) => l.t("кришка закрита", "lid closed"),
        (false, _) => l.t("приховано", "hidden"),
    };
    let (label, style) = if s.mirror_wanted {
        (l.t("Приховати", "Hide"), Style::Normal)
    } else {
        (l.t("Показати", "Show"), Style::Normal)
    };
    toggle_row(
        items,
        lx,
        lw,
        y,
        l.t("Екран ноутбука", "Laptop screen"),
        note,
        (label, style, Action::SetMirror(!s.mirror_wanted)),
    );
    y + 70.0
}

/// A row with a title, a muted note and one button on the right.
fn toggle_row(
    items: &mut Vec<Item>,
    lx: f32,
    lw: f32,
    y: f32,
    title: &str,
    note: &str,
    (label, style, action): (&str, Style, Action),
) {
    items.push(Item::Box(rect(lx, y, lw, 62.0), [255, 255, 255, 12]));
    items.push(text(lx + 20.0, y + 40.0, 24.0, true, TEXT, title));
    items.push(text(lx + 228.0, y + 40.0, 20.0, false, MUTED, note));
    items.push(button(
        rect(lx + lw - 10.0 - 180.0, y + 7.0, 180.0, 48.0),
        label,
        style,
        Some(action),
    ));
}

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
        items.push(text(x, *y, 21.0, false, MUTED, title));
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
fn build_monitor_options(s: &UiState, items: &mut Vec<Item>, rx: f32, rw: f32, mut y: f32) {
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
            (l.t("Дуга", "Arc"), Action::Preset(Preset::Arc), false, true),
            (
                l.t("Сітка", "Grid"),
                Action::Preset(Preset::Grid),
                false,
                true,
            ),
            (
                l.t("Стовпчик", "Column"),
                Action::Preset(Preset::Column),
                false,
                true,
            ),
            (
                l.t("Лежачи", "Lying down"),
                Action::Preset(Preset::Overhead),
                false,
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

    let mut y = top;
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
            "Кнопка A ставить усі монітори рівно. Утримання Meta перецентровує.",
            "The A button lines all monitors up. Holding the Meta button recenters.",
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

/// "1 монітор", "3 монітори", "5 моніторів" / "1 monitor", "3 monitors".
fn monitors_count(l: Lang, n: usize) -> String {
    let word = match l {
        Lang::En if n == 1 => "monitor",
        Lang::En => "monitors",
        Lang::Uk if n % 10 == 1 && n % 100 != 11 => "монітор",
        Lang::Uk if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) => "монітори",
        Lang::Uk => "моніторів",
    };
    format!("{n} {word}")
}

/// One card per profile: switch to it, or store the current setup in it.
fn build_profiles(s: &UiState, items: &mut Vec<Item>, x: f32, w: f32, mut y: f32) {
    let l = s.lang;
    let names = [
        l.t("Робота", "Work"),
        l.t("Кіно", "Cinema"),
        l.t("Лежачи", "Lying down"),
    ];
    for (i, p) in s.profiles.iter().enumerate() {
        items.push(Item::Box(rect(x, y, w, 104.0), [255, 255, 255, 12]));
        items.push(text(
            x + 24.0,
            y + 42.0,
            26.0,
            true,
            TEXT,
            names.get(i).copied().unwrap_or("—"),
        ));
        let mut sizes: Vec<String> = Vec::new();
        for (w, h) in &p.sizes {
            sizes.push(format!("{w}×{h}"));
        }
        let layout = if p.settings.cinema {
            l.t("кінотеатр", "cinema")
        } else {
            match p.settings.preset {
                Preset::Arc => l.t("дуга", "arc"),
                Preset::Grid => l.t("сітка", "grid"),
                Preset::Column => l.t("стовпчик", "column"),
                Preset::Overhead => l.t("лежачи", "lying down"),
            }
        };
        let mut summary = format!(
            "{}: {} · {layout}",
            monitors_count(l, p.sizes.len()),
            sizes.join(", ")
        );
        if p.mirror {
            summary.push_str(l.t(" · екран ноутбука", " · laptop screen"));
        }
        items.push(text(x + 24.0, y + 78.0, 19.0, false, MUTED, &summary));
        let save = rect(x + w - 16.0 - 230.0, y + 28.0, 230.0, 48.0);
        let apply = rect(save.left() - 10.0 - 200.0, y + 28.0, 200.0, 48.0);
        if s.active_profile == Some(i) {
            items.push(button(
                apply,
                l.t("Увімкнено", "Active"),
                Style::Selected,
                None,
            ));
        } else {
            let (style, action) = if s.host.is_some() {
                (Style::Normal, Some(Action::ApplyProfile(i)))
            } else {
                (Style::Disabled, None)
            };
            items.push(button(apply, l.t("Увімкнути", "Switch to"), style, action));
        }
        let (style, action) = if s.host.is_some() {
            (Style::Normal, Some(Action::SaveProfile(i)))
        } else {
            (Style::Disabled, None)
        };
        items.push(button(
            save,
            l.t("Зберегти поточне", "Save current"),
            style,
            action,
        ));
        y += 116.0;
    }
    y += 24.0;
    for line in [
        l.t(
            "«Зберегти поточне» записує в профіль теперішні монітори, екран ноутбука,",
            "“Save current” stores the current monitors, laptop screen,",
        ),
        l.t(
            "розкладку, вигин, фон і режим. Монітори того самого розміру",
            "layout, curve, background and mode. Monitors of the same size",
        ),
        l.t(
            "при перемиканні лишаються, тож вікна на них не зникають.",
            "are kept when switching, so their windows stay.",
        ),
    ] {
        items.push(text(x, y, 19.0, false, MUTED, line));
        y += 30.0;
    }
}

/// PC sound and headset microphone.
fn build_sound(s: &UiState, items: &mut Vec<Item>, lx: f32, lw: f32, mut y: f32) {
    let l = s.lang;
    let (note, label) = if s.audio_wanted {
        (
            l.t("у шоломі", "in the headset"),
            l.t("На ноутбук", "To the PC"),
        )
    } else {
        (
            l.t("на ноутбуці", "on the PC"),
            l.t("У шолом", "To the headset"),
        )
    };
    toggle_row(
        items,
        lx,
        lw,
        y,
        l.t("Звук", "Sound"),
        note,
        (label, Style::Normal, Action::SetAudio(!s.audio_wanted)),
    );
    y += 70.0;
    let (note, label) = if s.mic_wanted {
        (l.t("шолома", "headset"), l.t("З ноутбука", "Use the PC's"))
    } else {
        (l.t("ноутбука", "PC"), l.t("Зі шолома", "Use the headset's"))
    };
    toggle_row(
        items,
        lx,
        lw,
        y,
        l.t("Мікрофон", "Microphone"),
        note,
        (label, Style::Normal, Action::SetMic(!s.mic_wanted)),
    );
    y += 100.0;
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
            tab: Tab::Monitors,
            lang: Lang::Uk,
            intro: false,
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
            for tab in [Tab::Monitors, Tab::Profiles, Tab::Look, Tab::Sound] {
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
