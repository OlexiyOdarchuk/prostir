//! The floating menu panel. It is drawn on the CPU into an RGBA image and only
//! re-uploaded when something changes (hover, a click, news from the host).

use std::collections::HashMap;

use fontdue::{Font, FontSettings, Metrics};
use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Transform};

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

#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    /// Name of the connected PC, `None` while searching.
    pub host: Option<String>,
    /// Quest monitors, sorted by id.
    pub monitors: Vec<MonitorEntry>,
    pub primary: Option<u8>,
    pub mirror_wanted: bool,
    /// The physical display is being shown (its lid is open).
    pub mirror_active: bool,
    pub settings: Settings,
    pub room_available: bool,
    pub panorama_available: bool,
    /// The user put a `background.jpg/png` on the headset.
    pub custom_available: bool,
    pub curve_available: bool,
    pub error: Option<String>,
    /// Monitor whose "×" was pressed once and now asks for confirmation.
    pub confirm_remove: Option<u8>,
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
    Preset(Preset),
    Curve(Curve),
    Background(Background),
    Hands(HandMode),
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
        let mut items = vec![Item::Panel(rect(0.0, 0.0, WIDTH as f32, HEIGHT as f32))];
        let w = WIDTH as f32;

        // Header.
        items.push(text(44.0, 70.0, 34.0, true, TEXT, "Монітори в шоломі"));
        let status = match &s.host {
            Some(h) => format!("Підключено до {h}"),
            None => "Шукаю ПК…".to_owned(),
        };
        let sw = self.text_width(&status, 21.0, false);
        items.push(text(w - 120.0 - sw, 66.0, 21.0, false, MUTED, &status));
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

        // Left column: monitor list.
        let (lx, lw) = (44.0, 580.0);
        let mut y = 150.0;
        items.push(text(lx, y, 21.0, false, MUTED, "Монітори"));
        y += 18.0;
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
                &format!("Монітор {}", m.id + 1),
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
                    "Прибрати? Ще раз",
                    Style::Armed,
                    Some(Action::Remove(m.id)),
                ));
                y += 70.0;
                continue;
            }
            if s.primary == Some(m.id) {
                items.push(button(primary, "Головний", Style::Selected, None));
            } else {
                items.push(button(
                    primary,
                    "Зробити головним",
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
        items.push(Item::Box(rect(lx, y, lw, 62.0), [255, 255, 255, 12]));
        items.push(text(
            lx + 20.0,
            y + 40.0,
            24.0,
            true,
            TEXT,
            "Екран ноутбука",
        ));
        let note = match (s.mirror_wanted, s.mirror_active) {
            (true, true) => "показано",
            (true, false) => "кришка закрита",
            (false, _) => "приховано",
        };
        items.push(text(lx + 228.0, y + 40.0, 20.0, false, MUTED, note));
        let toggle = rect(lx + lw - 10.0 - 160.0, y + 7.0, 160.0, 48.0);
        let (label, style) = if s.mirror_wanted {
            ("Приховати", Style::Normal)
        } else {
            ("Показати", Style::Normal)
        };
        items.push(button(
            toggle,
            label,
            style,
            Some(Action::SetMirror(!s.mirror_wanted)),
        ));
        y += 70.0;

        // Hints under the list.
        y = y.max(560.0) + 30.0;
        for line in [
            "Контролер: курок — натиснути, grip — перенести",
            "Стік, поки тримаєте, — відстань і розмір · A — вирівняти",
            "Руки: щипок — натиснути, щипок і тримати — перенести",
            "Відсуньте або наблизьте руку — монітор далі чи ближче",
        ] {
            items.push(text(lx, y, 19.0, false, MUTED, line));
            y += 30.0;
        }
        if let Some(e) = &s.error {
            items.push(text(lx, HEIGHT as f32 - 34.0, 21.0, true, DANGER, e));
        }

        // Right column: add, layout, curve, background.
        let (rx, rw) = (680.0, 556.0);
        let mut y = 150.0;
        items.push(text(rx, y, 21.0, false, MUTED, "Додати монітор"));
        y += 18.0;
        let full = s.monitors.len() >= MAX_MONITORS;
        let sizes = [
            (1920, 1080, "1920 × 1080"),
            (2560, 1440, "2560 × 1440"),
            (3840, 2160, "3840 × 2160"),
            (3440, 1440, "3440 × 1440 широкий"),
            (1440, 2560, "1440 × 2560 портрет"),
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

        let segmented = |items: &mut Vec<Item>,
                         y: &mut f32,
                         title: &str,
                         options: Vec<(&str, Action, bool, bool)>| {
            if !title.is_empty() {
                items.push(text(rx, *y, 21.0, false, MUTED, title));
                *y += 18.0;
            } else {
                // Second row of the previous group.
                *y -= 30.0;
            }
            let n = options.len() as f32;
            let bw = (rw - 10.0 * (n - 1.0)) / n;
            for (i, (label, action, selected, enabled)) in options.into_iter().enumerate() {
                let r = rect(rx + i as f32 * (bw + 10.0), *y, bw, 48.0);
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
        };
        let st = s.settings;
        segmented(
            &mut items,
            &mut y,
            "Розкладка",
            vec![
                ("Дуга", Action::Preset(Preset::Arc), false, true),
                ("Сітка", Action::Preset(Preset::Grid), false, true),
                ("Стовпчик", Action::Preset(Preset::Column), false, true),
                ("Над головою", Action::Preset(Preset::Overhead), false, true),
            ],
        );
        segmented(
            &mut items,
            &mut y,
            "Вигин",
            vec![
                (
                    "Пласкі",
                    Action::Curve(Curve::Flat),
                    st.curve == Curve::Flat,
                    true,
                ),
                (
                    "Легкий",
                    Action::Curve(Curve::Gentle),
                    st.curve == Curve::Gentle,
                    s.curve_available,
                ),
                (
                    "Сильний",
                    Action::Curve(Curve::Strong),
                    st.curve == Curve::Strong,
                    s.curve_available,
                ),
            ],
        );
        let bg = |b: Background| (Action::Background(b), st.background == b);
        let (a1, s1) = bg(Background::Black);
        let (a2, s2) = bg(Background::Room);
        let (a3, s3) = bg(Background::Custom);
        segmented(
            &mut items,
            &mut y,
            "Фон",
            vec![
                ("Чорний", a1, s1, true),
                ("Кімната", a2, s2, s.room_available),
                (
                    "Своє фото",
                    a3,
                    s3,
                    s.panorama_available && s.custom_available,
                ),
            ],
        );
        let skies = [
            ("Ніч", Sky::Night),
            ("Захід", Sky::Sunset),
            ("Космос", Sky::Space),
            ("Сяйво", Sky::Aurora),
        ];
        segmented(
            &mut items,
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
            &mut items,
            &mut y,
            "Руки",
            vec![
                (
                    "Сховати",
                    Action::Hands(HandMode::Hidden),
                    st.hands == HandMode::Hidden,
                    true,
                ),
                (
                    "Показувати",
                    Action::Hands(HandMode::Visible),
                    st.hands == HandMode::Visible,
                    true,
                ),
                (
                    "І керувати",
                    Action::Hands(HandMode::Control),
                    st.hands == HandMode::Control,
                    true,
                ),
            ],
        );
        items
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
                    let size = match label.as_str() {
                        "×" => 32.0,
                        "‹" | "›" => 34.0,
                        _ => 19.0,
                    };
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
            host: Some("iShawyha-Laptop".into()),
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
            mirror_active: true,
            settings: Settings::default(),
            room_available: true,
            panorama_available: true,
            custom_available: false,
            curve_available: true,
            error: None,
            confirm_remove: None,
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
}
