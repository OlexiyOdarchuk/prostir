//! The monitor map. Flat (the main view): the displays as the PC's desktop has them,
//! moved by dragging or with the arrow keys, each landing against a neighbour
//! ([`crate::snap`]). Around you: the same displays on an arc around the viewer, to
//! look at only; dragging an empty spot turns the view.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Key, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};

use crate::snap;
use crate::theme::mix;
use crate::widgets::{Tk, faded};

/// One display as the map draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    pub id: u32,
    pub name: String,
    /// Width and height as written under the name.
    pub size: String,
    pub rect: snap::Rect,
    /// A screen of the PC itself (drawn with an outline: hardware, not a stream).
    pub pc: bool,
    /// Shown in the headset; dimmed when not.
    pub shown: bool,
    pub main: bool,
}

/// What happened on the map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapEvent {
    Select(u32),
    Place { id: u32, x: i32, y: i32 },
}

/// The tile being dragged: its id, where the pointer has it, where it would land.
type Ghost = (u32, (i32, i32), Option<(i32, i32)>);

struct Drag {
    id: u32,
    /// Where the pointer took the tile, in desktop pixels from its top-left.
    grab: Vec2,
    scale: f32,
    origin: Pos2,
    at: Pos2,
}

/// Pulled into line within this many points on screen.
const MAGNET_POINTS: f32 = 10.0;

pub struct MapState {
    drag: Option<Drag>,
    yaw: f32,
    pitch: f32,
}

impl Default for MapState {
    fn default() -> Self {
        Self {
            drag: None,
            yaw: 0.0,
            pitch: 0.28,
        }
    }
}

fn bounds(tiles: &[Tile]) -> Rect {
    tiles.iter().fold(Rect::NOTHING, |b, t| {
        b.union(Rect::from_min_size(
            pos2(t.rect.x as f32, t.rect.y as f32),
            vec2(t.rect.w as f32, t.rect.h as f32),
        ))
    })
}

fn label(ui: &Ui, tk: &Tk, r: Rect, t: &Tile, color: Color32) {
    let c = r.center();
    // The name: smaller on a narrow monitor, and shortened with an ellipsis where
    // even that does not fit.
    let room = r.width() - 8.0;
    if room < tk.th.type_.size_small || r.height() < tk.th.type_.size_small + 4.0 {
        return;
    }
    let fits = |font| {
        let g = ui.painter().layout_no_wrap(t.name.clone(), font, color);
        (g.size().x <= room).then_some(g)
    };
    let name = fits(tk.body_m())
        .or_else(|| fits(tk.small()))
        .unwrap_or_else(|| {
            let mut job =
                egui::text::LayoutJob::simple_singleline(t.name.clone(), tk.small(), color);
            job.wrap = egui::text::TextWrapping {
                max_width: room,
                max_rows: 1,
                break_anywhere: true,
                overflow_character: Some('…'),
            };
            ui.painter().layout_job(job)
        });
    if r.height() < 52.0 || name.size().y < tk.th.type_.size_body {
        ui.painter().galley(c - name.size() / 2.0, name, color);
    } else {
        ui.painter()
            .galley(c - name.size() / 2.0 - vec2(0.0, 10.0), name, color);
        ui.painter().text(
            c + vec2(0.0, 11.0),
            Align2::CENTER_CENTER,
            &t.size,
            tk.small(),
            faded(color, 0.85),
        );
    }
}

fn colors(tk: &Tk, t: &Tile, selected: bool, hover: f32) -> (Color32, Color32) {
    let p = tk.p;
    if selected {
        (p.accent, p.accent_text)
    } else if t.pc {
        (p.screen_pc, p.screen_text)
    } else {
        (mix(p.screen, p.control_hover, hover * 0.5), p.screen_text)
    }
}

/// The flat map.
pub fn flat(
    ui: &mut Ui,
    tk: &Tk,
    st: &mut MapState,
    tiles: &[Tile],
    selected: Option<u32>,
    height: f32,
) -> Vec<MapEvent> {
    let mut events = Vec::new();
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let p = tk.p;
    ui.painter().rect_filled(rect, tk.r_large(), p.well);
    if tiles.is_empty() {
        return events;
    }
    if st
        .drag
        .as_ref()
        .is_some_and(|d| !tiles.iter().any(|t| t.id == d.id))
    {
        st.drag = None;
    }
    // Fit while at rest; keep the scale while dragging so the map does not swim.
    let (scale, origin) = if let Some(d) = &st.drag {
        (d.scale, d.origin)
    } else {
        let bb = bounds(tiles);
        let pad = tk.th.shape.padding * 2.0;
        let s = ((rect.width() - pad * 2.0) / bb.width())
            .min((rect.height() - pad * 2.0) / bb.height())
            .min(0.12);
        (
            s,
            rect.center() - bb.size() * s / 2.0 - bb.min.to_vec2() * s,
        )
    };
    let magnet = (MAGNET_POINTS / scale).round() as i32;
    let to_screen = |r: snap::Rect| {
        Rect::from_min_size(
            origin + vec2(r.x as f32, r.y as f32) * scale,
            vec2(r.w as f32, r.h as f32) * scale,
        )
    };
    // Tiles are drawn with a gap between them, so touching monitors still read as two.
    let gap = 3.0;
    let others = |id: u32| -> Vec<snap::Rect> {
        tiles
            .iter()
            .filter(|t| t.id != id)
            .map(|t| t.rect)
            .collect()
    };

    // Where the dragged tile is under the pointer, and where it would land.
    let mut ghost: Option<Ghost> = None;
    if let Some(d) = &st.drag
        && let Some(t) = tiles.iter().find(|t| t.id == d.id)
    {
        let want = ((d.at - origin) / scale - d.grab).round();
        let want = (want.x as i32, want.y as i32);
        let land = snap::snap(&others(t.id), (t.rect.w, t.rect.h), want, magnet);
        ghost = Some((t.id, want, land));
    }

    let mut order: Vec<&Tile> = tiles.iter().collect();
    if let Some((id, ..)) = ghost {
        order.sort_by_key(|t| t.id == id);
    }
    for t in order {
        let id = ui.scope_id().with(("monitor", t.id));
        let dragging = ghost.filter(|g| g.0 == t.id);
        let r = match dragging {
            Some((_, want, _)) => to_screen(t.rect.at(want)),
            None => to_screen(t.rect),
        }
        .shrink(gap);
        let resp = ui.interact(r, id, Sense::click_and_drag());
        let h = ui.ctx().animate_bool_with_time(
            id.with("h"),
            resp.hovered(),
            tk.secs(tk.th.motion.hover_ms),
        );
        let on = selected == Some(t.id);
        let (fill, fg) = colors(tk, t, on, h);
        if dragging.is_some() {
            // The one lifted thing on the page: it is in the hand, not on the desk.
            ui.painter().add(
                egui::epaint::RectShape::filled(r.translate(vec2(0.0, 6.0)), tk.r(), p.shadow)
                    .with_blur_width(18.0),
            );
        }
        let k = if t.shown { 1.0 } else { 0.5 };
        ui.painter().rect(
            r,
            tk.r(),
            faded(fill, k),
            if t.pc {
                Stroke::new(
                    tk.th.shape.stroke * 1.5,
                    mix(p.control_border, p.accent_ink, h),
                )
            } else {
                Stroke::NONE
            },
            StrokeKind::Inside,
        );
        if t.main {
            // The panel lives on the main monitor: a short bar where it sits.
            let bar = Rect::from_center_size(
                pos2(r.center().x, r.bottom() - 7.0),
                vec2(r.width() * 0.4, 3.0),
            );
            ui.painter()
                .rect_filled(bar, CornerRadius::same(2), faded(fg, 0.7));
        }
        label(ui, tk, r, t, faded(fg, k));
        tk.focus_ring(ui, &resp, tk.th.shape.radius);
        resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, on, &t.name));

        if resp.clicked() {
            events.push(MapEvent::Select(t.id));
        }
        if resp.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        ..Default::default()
                    },
                )
            });
            let dir = ui.input(|i| {
                [
                    (Key::ArrowLeft, (-1, 0)),
                    (Key::ArrowRight, (1, 0)),
                    (Key::ArrowUp, (0, -1)),
                    (Key::ArrowDown, (0, 1)),
                ]
                .into_iter()
                .find(|(k, _)| i.key_pressed(*k))
                .map(|(_, d)| d)
            });
            if let Some(d) = dir {
                if !on {
                    events.push(MapEvent::Select(t.id));
                }
                if let Some((x, y)) = snap::step(&others(t.id), t.rect, d, 0) {
                    events.push(MapEvent::Place { id: t.id, x, y });
                }
            }
        }
        if resp.drag_started()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            if !on {
                events.push(MapEvent::Select(t.id));
            }
            let grab = (pos - origin) / scale - vec2(t.rect.x as f32, t.rect.y as f32);
            st.drag = Some(Drag {
                id: t.id,
                grab,
                scale,
                origin,
                at: pos,
            });
        }
        if resp.dragged()
            && let (Some(d), Some(pos)) = (&mut st.drag, resp.interact_pointer_pos())
        {
            d.at = pos;
        }
        if resp.drag_stopped() {
            if let Some((id, _, Some((x, y)))) = ghost
                && (x, y) != (t.rect.x, t.rect.y)
            {
                events.push(MapEvent::Place { id, x, y });
            }
            st.drag = None;
        }
    }
    if let Some((id, _, Some(land))) = ghost
        && let Some(t) = tiles.iter().find(|t| t.id == id)
    {
        let r = to_screen(t.rect.at(land)).shrink(gap);
        ui.painter().rect_stroke(
            r,
            tk.r(),
            Stroke::new(tk.th.shape.focus_width, p.focus),
            StrokeKind::Inside,
        );
    }
    if st.drag.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    events
}

type V3 = [f32; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot3(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm(a: V3) -> V3 {
    let l = dot3(a, a).sqrt();
    [a[0] / l, a[1] / l, a[2] / l]
}

fn inside(pts: &[Pos2], at: Pos2) -> bool {
    let n = pts.len();
    let side = |i: usize| {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        (b - a).x * (at - a).y - (b - a).y * (at - a).x
    };
    (0..n).all(|i| side(i) >= 0.0) || (0..n).all(|i| side(i) <= 0.0)
}

/// The displays on an arc around the viewer, seen from behind and above.
pub fn arc(
    ui: &mut Ui,
    tk: &Tk,
    st: &mut MapState,
    tiles: &[Tile],
    selected: Option<u32>,
    height: f32,
) -> Vec<MapEvent> {
    let mut events = Vec::new();
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click_and_drag());
    let p = tk.p;
    ui.painter().rect_filled(rect, tk.r_large(), p.well);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, tk.t("mon-arc")));
    if resp.dragged() {
        let d = resp.drag_delta();
        st.yaw = (st.yaw - d.x * 0.006).clamp(-1.2, 1.2);
        st.pitch = (st.pitch + d.y * 0.004).clamp(0.05, 1.1);
    }
    if resp.double_clicked() {
        *st = MapState::default();
    }
    let Some(centre) = tiles.iter().find(|t| t.main).or(tiles.first()) else {
        return events;
    };

    // Desktop pixels to metres: a 2560 wide monitor is about 0.95 m on a 1.25 m arc.
    let k = 0.95 / 2560.0;
    let radius = 1.25;
    let cx0 = centre.rect.x as f32 + centre.rect.w as f32 / 2.0;
    let cy0 = centre.rect.y as f32 + centre.rect.h as f32 / 2.0;
    let quad = |t: &Tile| -> [V3; 4] {
        let r = t.rect;
        // A little more angle, so neighbours on the arc do not cut into each other.
        let s = (r.x as f32 + r.w as f32 / 2.0 - cx0) * k * 1.04;
        let th = s / radius;
        let y = -(r.y as f32 + r.h as f32 / 2.0 - cy0) * k;
        let c = [radius * th.sin(), y, -radius * th.cos()];
        let tangent = [th.cos(), 0.0, th.sin()];
        let (hw, hh) = (r.w as f32 * k / 2.0, r.h as f32 * k / 2.0);
        let at = |a: f32, b: f32| [c[0] + tangent[0] * a, c[1] + b, c[2] + tangent[2] * a];
        [at(-hw, hh), at(hw, hh), at(hw, -hh), at(-hw, -hh)]
    };
    let target = [0.0, -0.1, -0.8];
    let dist = 2.3;
    let cam = [
        target[0] + dist * st.yaw.sin() * st.pitch.cos(),
        target[1] + dist * st.pitch.sin(),
        target[2] + dist * st.yaw.cos() * st.pitch.cos(),
    ];
    let f = norm(sub(target, cam));
    let right = norm(cross(f, [0.0, 1.0, 0.0]));
    let up = cross(right, f);
    let proj = |pt: V3| -> (Vec2, f32) {
        let v = sub(pt, cam);
        let z = dot3(v, f);
        (vec2(dot3(v, right) / z, -dot3(v, up) / z), z)
    };

    let floor_y = tiles
        .iter()
        .map(|t| -(t.rect.y as f32 + t.rect.h as f32 - cy0) * k)
        .fold(0.0, f32::min)
        - 0.12;
    let floor: Vec<V3> = (0..=40)
        .map(|i| {
            let a = (-80.0 + 160.0 * i as f32 / 40.0f32).to_radians();
            [radius * a.sin(), floor_y, -radius * a.cos()]
        })
        .collect();
    let quads: Vec<[V3; 4]> = tiles.iter().map(quad).collect();
    let mut bb = Rect::NOTHING;
    for v in quads.iter().flatten().chain(&floor) {
        bb.extend_with(proj(*v).0.to_pos2());
    }
    let pad = tk.th.shape.padding * 2.0;
    let s =
        ((rect.width() - pad * 2.0) / bb.width()).min((rect.height() - pad * 2.0) / bb.height());
    let off = rect.center() - bb.center().to_vec2() * s;
    let scr = |pt: V3| off + proj(pt).0 * s;

    // The floor arc and the viewer: what "around you" is around.
    let line: Vec<Pos2> = floor.iter().map(|v| scr(*v)).collect();
    ui.painter().add(Shape::line(
        line,
        Stroke::new(tk.th.shape.stroke * 1.5, p.border),
    ));
    let head = scr([0.0, floor_y, 0.0]);
    let facing = scr([0.0, floor_y, -0.3]);
    ui.painter().line_segment(
        [head, facing],
        Stroke::new(tk.th.shape.focus_width, p.text_muted),
    );
    ui.painter()
        .circle_filled(head, tk.th.type_.size_body * 0.45, p.text_muted);

    let depth = |q: &[V3; 4]| {
        let c = [
            (q[0][0] + q[2][0]) / 2.0,
            (q[0][1] + q[2][1]) / 2.0,
            (q[0][2] + q[2][2]) / 2.0,
        ];
        proj(c).1
    };
    let mut order: Vec<usize> = (0..tiles.len()).collect();
    order.sort_by(|&a, &b| depth(&quads[b]).total_cmp(&depth(&quads[a])));
    let shapes: Vec<Vec<Pos2>> = quads
        .iter()
        .map(|q| q.iter().map(|v| scr(*v)).collect())
        .collect();
    let hit = resp
        .hover_pos()
        .and_then(|h| order.iter().rev().copied().find(|&i| inside(&shapes[i], h)));
    for &i in &order {
        let t = &tiles[i];
        let pts = &shapes[i];
        let on = selected == Some(t.id);
        let hov = hit == Some(i) && !resp.dragged();
        let (fill, fg) = colors(tk, t, on, if hov { 1.0 } else { 0.0 });
        let kk = if t.shown { 1.0 } else { 0.5 };
        let stroke = if t.pc || hov {
            Stroke::new(
                tk.th.shape.stroke * 1.5,
                if hov { p.accent_ink } else { p.control_border },
            )
        } else {
            Stroke::NONE
        };
        ui.painter()
            .add(Shape::convex_polygon(pts.clone(), faded(fill, kk), stroke));
        let c = pts.iter().fold(Vec2::ZERO, |a, p| a + p.to_vec2()) / 4.0;
        // The name only where it fits on the screen's face.
        let face = pts[0].distance(pts[1]).min(pts[3].distance(pts[2])) - 8.0;
        for font in [tk.body_m(), tk.small()] {
            let g = ui
                .painter()
                .layout_no_wrap(t.name.clone(), font, faded(fg, kk));
            if g.size().x <= face {
                ui.painter().galley(c.to_pos2() - g.size() / 2.0, g, fg);
                break;
            }
        }
    }
    if resp.clicked()
        && let Some(i) = hit
    {
        events.push(MapEvent::Select(tiles[i].id));
    }
    if resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else if hit.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    } else if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    events
}
