//! The pages. Each draws from the service's state and says what it wants done as
//! [`Action`]s; the window carries them out after the frame.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Rect, Sense, Ui, pos2, vec2};
use prostir_lang::{Arg, Language};

use crate::api::*;
use crate::app::{Conn, Outcome, Page, Start, UiState};
use crate::map::{self, MapEvent, Tile};
use crate::snap;
use crate::theme::{IssueKind, PaletteName, Scheme};
use crate::widgets::{self as w, Kind, Tk, text};

/// Where the releases are, for "Install Prostir in the headset".
pub const RELEASES: &str = "https://github.com/OlexiyOdarchuk/prostir/releases";

/// Sizes offered when the state names none.
const SIZES: [[u32; 2]; 5] = [
    [1920, 1080],
    [2560, 1440],
    [3840, 2160],
    [3440, 1440],
    [1440, 2560],
];
const SCALES: [u16; 3] = [100, 125, 150];

pub enum Action {
    Send(Request),
    /// Ask the service for its whole state again.
    Refresh,
    Go(Page),
    Look(WindowLook),
    Start,
    OpenUrl(String),
    OpenFolder(PathBuf),
    CreateThemeFile(PathBuf),
    Dismiss,
}

/// What a page sees and where it puts what it wants.
pub struct View<'a, 'b> {
    pub tk: &'a Tk<'b>,
    pub conn: &'a Conn,
    pub ui: &'a mut UiState,
    pub out: &'a mut Vec<Action>,
    /// The window's time in seconds.
    pub now: f64,
    pub now_unix: u64,
    pub look: &'a WindowLook,
    pub languages: &'a [Language],
    pub lang_dirs: &'a [PathBuf],
    pub system: Option<Scheme>,
    pub theme_file: Option<&'a Path>,
    pub theme_file_exists: bool,
}

impl View<'_, '_> {
    fn send(&mut self, r: Request) {
        self.out.push(Action::Send(r));
    }
}

pub fn show(ui: &mut Ui, v: &mut View) {
    let page = v.ui.page;
    if page == Page::Look {
        return look(ui, v);
    }
    let conn = v.conn;
    match conn {
        Conn::Live { snap, .. } => {
            let s = &snap.state;
            match page {
                Page::Overview => overview(ui, v, s),
                Page::Monitors => monitors(ui, v, s, snap.revision),
                Page::Headsets => headsets(ui, v, s),
                Page::Sound => sound(ui, v, s),
                Page::FirstRun => first_run(ui, v, s),
                Page::Look => unreachable!(),
            }
        }
        Conn::Loading => {
            header(ui, v, page);
            w::card(ui, v.tk, |ui| {
                ui.horizontal(|ui| {
                    w::spinner(ui, v.tk, v.tk.th.type_.size_title);
                    text(ui, v.tk.t("loading-title"), v.tk.heading(), v.tk.p.text);
                });
            });
        }
        Conn::Absent { why, start } => {
            header(ui, v, page);
            not_running(ui, v, why, start);
        }
    }
}

fn header(ui: &mut Ui, v: &View, page: Page) {
    let (title, sub) = match page {
        Page::Overview => ("ov-title", None),
        Page::Monitors => ("mon-title", Some("mon-sub")),
        Page::Headsets => ("hs-title", Some("hs-sub")),
        Page::Sound => ("snd-title", Some("snd-sub")),
        Page::Look => ("look-title", Some("look-sub")),
        Page::FirstRun => ("fr-title", None),
    };
    let sub = sub.map(|s| v.tk.t(s));
    w::page_header(ui, v.tk, &v.tk.t(title), sub.as_deref());
}

/// A large title with a coloured dot for the state in front of it.
fn hero(ui: &mut Ui, tk: &Tk, color: Option<Color32>, title: &str) {
    ui.horizontal(|ui| {
        if let Some(c) = color {
            let (r, _) =
                ui.allocate_exact_size(vec2(16.0, tk.th.type_.size_display * 1.2), Sense::hover());
            ui.painter()
                .circle_filled(r.center(), tk.th.type_.size_body * 0.4, c);
            ui.add_space(4.0);
        }
        text(
            ui,
            title,
            tk.font(tk.th.type_.size_display, true),
            tk.p.text,
        );
    });
}

fn not_running(ui: &mut Ui, v: &mut View, why: &ClientError, start: &Start) {
    let tk = v.tk;
    let p = tk.p;
    w::card(ui, tk, |ui| {
        if let ClientError::NoBus(_) = why {
            hero(ui, tk, Some(p.danger), &tk.t("nr-title"));
            ui.add_space(6.0);
            text(ui, tk.t("nr-no-bus"), tk.body(), p.text_muted);
            return;
        }
        if silent(why) {
            hero(ui, tk, Some(p.warning), &tk.t("nr-silent-title"));
            ui.add_space(6.0);
            text(ui, tk.t("nr-silent-body"), tk.body(), p.text_muted);
            ui.add_space(tk.th.shape.padding);
            if w::button(ui, tk, &tk.t("nr-retry"), Kind::Secondary, true).clicked() {
                v.out.push(Action::Refresh);
            }
            return;
        }
        match start {
            Start::Waiting { .. } => {
                ui.horizontal(|ui| {
                    w::spinner(ui, tk, tk.th.type_.size_display);
                    ui.add_space(4.0);
                    text(
                        ui,
                        tk.t("nr-starting"),
                        tk.font(tk.th.type_.size_display, true),
                        p.text,
                    );
                });
                ui.add_space(6.0);
                text(ui, tk.t("nr-body"), tk.body(), p.text_muted);
            }
            Start::AlreadyRunning => {
                hero(ui, tk, Some(p.warning), &tk.t("nr-old-title"));
                ui.add_space(6.0);
                text(ui, tk.t("nr-old-body"), tk.body(), p.text_muted);
                ui.add_space(tk.th.shape.padding);
                if w::button(ui, tk, &tk.t("nr-retry"), Kind::Secondary, true).clicked() {
                    v.out.push(Action::Refresh);
                }
            }
            Start::Idle | Start::NoProgram | Start::TimedOut => {
                let failed = *start != Start::Idle;
                let title = if failed {
                    "nr-failed-title"
                } else {
                    "nr-title"
                };
                hero(
                    ui,
                    tk,
                    Some(if failed { p.danger } else { p.warning }),
                    &tk.t(title),
                );
                ui.add_space(6.0);
                let body = match start {
                    Start::NoProgram => "nr-no-program",
                    Start::TimedOut => "nr-failed-body",
                    _ => "nr-body",
                };
                text(ui, tk.t(body), tk.body(), p.text_muted);
                ui.add_space(tk.th.shape.padding);
                if w::button(ui, tk, &tk.t("nr-start"), Kind::Primary, true).clicked() {
                    v.out.push(Action::Start);
                }
            }
        }
    });
}

/// The service is there but did not answer (rather than not there at all).
pub fn silent(why: &ClientError) -> bool {
    matches!(
        why,
        ClientError::TimedOut | ClientError::Broken(_) | ClientError::Refused { .. }
    )
}

fn percent(tk: &Tk, n: impl Into<Arg>) -> String {
    tk.f("percent", &[("n", n.into())])
}

fn size(tk: &Tk, m: &crate::api::Mode) -> String {
    tk.f("mon-size", &[("w", m.w.into()), ("h", m.h.into())])
}

/// How a display is named to a person.
fn display_name(tk: &Tk, d: &Display) -> String {
    match (d.kind, d.builtin) {
        (DisplayKind::Physical, true) => tk.t("mon-laptop"),
        (DisplayKind::Physical, false) if d.name.is_empty() => tk.t("mon-pc-screen"),
        _ => d.name.clone(),
    }
}

fn ago(tk: &Tk, now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..60 => tk.t("ago-now"),
        60..3600 => tk.f("ago-minutes", &[("n", (s / 60).into())]),
        3600..86400 => tk.f("ago-hours", &[("n", (s / 3600).into())]),
        _ => tk.f("ago-days", &[("n", (s / 86400).into())]),
    }
}

// ------------------------------------------------------------------------- overview

fn overview(ui: &mut Ui, v: &mut View, s: &State) {
    let tk = v.tk;
    let p = tk.p;
    header(ui, v, Page::Overview);
    let gap = tk.th.shape.section_gap;
    let virtual_count = s
        .displays
        .iter()
        .filter(|d| d.kind == DisplayKind::Virtual && d.state != DisplayState::Failed)
        .count();
    let failed = s.displays.iter().find(|d| d.state == DisplayState::Failed);

    w::card(ui, tk, |ui| {
        if let Some((h, session)) = s.connected() {
            let full = ui.available_width();
            ui.allocate_ui_with_layout(vec2(full, 0.0), Layout::right_to_left(Align::Min), |ui| {
                if let Some(b) = session.battery {
                    ui.allocate_ui_with_layout(
                        vec2(170.0, 0.0),
                        Layout::top_down(Align::Max),
                        |ui| {
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                w::battery(ui, tk, b.percent as f32 / 100.0, b.charging);
                                ui.add_space(4.0);
                                text(
                                    ui,
                                    percent(tk, b.percent),
                                    tk.font(tk.th.type_.size_title, true),
                                    p.text,
                                );
                            });
                            let label = if b.charging {
                                "ov-charging"
                            } else {
                                "ov-battery"
                            };
                            text(ui, tk.t(label), tk.small(), p.text_muted);
                        },
                    );
                }
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    let name = || ("name", Arg::from(&h.name));
                    if let Some(d) = failed {
                        hero(ui, tk, Some(p.danger), &tk.t("ov-failed-title"));
                        ui.add_space(6.0);
                        let reason = d.failure.as_ref().map_or("", |f| f.text.trim());
                        let id = if reason.is_empty() {
                            "ov-failed-no-reason"
                        } else {
                            "ov-failed-body"
                        };
                        let body = tk.f(
                            id,
                            &[
                                ("name", display_name(tk, d).into()),
                                ("reason", reason.into()),
                            ],
                        );
                        text(ui, body, tk.body(), p.text_muted);
                        ui.add_space(tk.th.shape.padding);
                        ui.horizontal(|ui| {
                            if w::button(ui, tk, &tk.t("ov-retry"), Kind::Primary, true).clicked() {
                                v.out.push(Action::Send(Request::RetryMonitor { id: d.id }));
                            }
                            if w::button(ui, tk, &tk.t("ov-open-monitors"), Kind::Secondary, true)
                                .clicked()
                            {
                                v.out.push(Action::Go(Page::Monitors));
                            }
                        });
                    } else if !session.worn {
                        hero(ui, tk, Some(p.text_muted), &tk.t("ov-taken-off-title"));
                        ui.add_space(6.0);
                        text(
                            ui,
                            tk.f("ov-taken-off-body", &[name()]),
                            tk.body(),
                            p.text_muted,
                        );
                    } else {
                        hero(ui, tk, Some(p.ok), &tk.t("ov-connected-title"));
                        ui.add_space(6.0);
                        let id = match session.via {
                            Via::Usb => "ov-connected-usb",
                            _ => "ov-connected-wifi",
                        };
                        let body = tk.f(id, &[name(), ("count", virtual_count.into())]);
                        text(ui, body, tk.body(), p.text_muted);
                    }
                });
            });
            w::divider(ui, tk);
            let mic = if !s.sound.mic_on {
                tk.t("off")
            } else {
                tk.t("on")
            };
            let facts = [
                (
                    tk.t("ov-fact-sound"),
                    tk.t(if s.sound.to_headset {
                        "sound-in-headset"
                    } else {
                        "sound-on-pc"
                    }),
                ),
                (tk.t("ov-fact-mic"), mic),
                (
                    tk.t("ov-fact-profile"),
                    session
                        .profile
                        .clone()
                        .unwrap_or_else(|| tk.t("ov-profile-none")),
                ),
            ];
            ui.columns(3, |c| {
                for (i, (k, val)) in facts.into_iter().enumerate() {
                    text(&mut c[i], k, tk.small(), p.text_muted);
                    text(&mut c[i], val, tk.body_m(), p.text);
                }
            });
        } else if let Some(name) = &s.connecting {
            ui.horizontal(|ui| {
                w::spinner(ui, tk, tk.th.type_.size_display);
                ui.add_space(4.0);
                text(
                    ui,
                    tk.f("ov-connecting-title", &[("name", name.into())]),
                    tk.font(tk.th.type_.size_display, true),
                    p.text,
                );
            });
            ui.add_space(6.0);
            text(ui, tk.t("ov-connecting-body"), tk.body(), p.text_muted);
        } else if s.headsets.is_empty() {
            hero(ui, tk, None, &tk.t("ov-no-headsets-title"));
            ui.add_space(6.0);
            text(ui, tk.t("ov-no-headsets-body"), tk.body(), p.text_muted);
            ui.add_space(tk.th.shape.padding);
            if w::button(ui, tk, &tk.t("ov-no-headsets-go"), Kind::Primary, true).clicked() {
                v.out.push(Action::Go(Page::FirstRun));
            }
        } else {
            hero(ui, tk, Some(p.text_muted), &tk.t("ov-waiting-title"));
            ui.add_space(6.0);
            text(ui, tk.t("ov-waiting-body"), tk.body(), p.text_muted);
            ui.add_space(tk.th.shape.padding);
            if w::button(ui, tk, &tk.t("ov-pair-new"), Kind::Secondary, true).clicked() {
                if s.pairing.is_none() {
                    v.out.push(Action::Send(Request::PairNewHeadset));
                }
                v.out.push(Action::Go(Page::Headsets));
            }
        }
    });

    if !s.problems.is_empty() {
        ui.add_space(gap);
        w::section_title(ui, tk, &tk.t("ov-problems"));
        w::card(ui, tk, |ui| {
            for pr in &s.problems {
                w::status(ui, tk, p.warning, &pr.text);
            }
        });
    }

    ui.add_space(gap);
    w::section_title(ui, tk, &tk.t("ov-monitors"));
    w::card(ui, tk, |ui| {
        if s.connected().is_none() {
            text(ui, tk.t("ov-monitors-empty"), tk.body(), p.text_muted);
            return;
        }
        let n = s.displays.len();
        for (i, d) in s.displays.iter().enumerate() {
            let (c, state) = match (d.state, d.kind) {
                (DisplayState::Failed, _) => (p.danger, "state-failed"),
                (DisplayState::Starting, _) => (p.accent_ink, "state-starting"),
                (DisplayState::Absent, _) => (p.text_muted, "state-absent"),
                (_, DisplayKind::Physical) if !d.shown => (p.text_muted, "state-hidden"),
                (_, DisplayKind::Physical) => (p.ok, "state-shown"),
                _ => (p.ok, "state-streaming"),
            };
            let desc = size(tk, &d.mode);
            w::row(ui, tk, &display_name(tk, d), Some(&desc), |ui| {
                text(ui, tk.t(state), tk.body(), p.text);
                let (r, _) = ui.allocate_exact_size(vec2(12.0, 20.0), Sense::hover());
                w::dot(ui, tk, r.center(), c);
            });
            if i + 1 < n {
                w::divider(ui, tk);
            }
        }
    });

    ui.add_space(gap);
    w::section_title(ui, tk, &tk.t("ov-pc"));
    w::card(ui, tk, |ui| {
        w::row(ui, tk, &tk.t("ov-pc-name"), None, |ui| {
            text(ui, &s.pc.name, tk.body(), p.text);
        });
        if !s.pc.desktop.is_empty() {
            w::divider(ui, tk);
            w::row(ui, tk, &tk.t("ov-pc-desktop"), None, |ui| {
                text(ui, &s.pc.desktop, tk.body(), p.text);
            });
        }
        if let Some(on) = s.autostart {
            w::divider(ui, tk);
            let label = tk.t("ov-autostart");
            w::row(ui, tk, &label, Some(&tk.t("ov-autostart-desc")), |ui| {
                if let Some(on) = w::switch(ui, tk, &label, on, true) {
                    v.out.push(Action::Send(Request::Autostart { on }));
                }
            });
        }
    });
}

// ------------------------------------------------------------------------- monitors

fn tiles(tk: &Tk, s: &State, pending: Option<(u32, i32, i32, u32)>) -> Vec<Tile> {
    s.displays
        .iter()
        .filter_map(|d| {
            let r = d.desk?;
            if d.state == DisplayState::Failed || r.w == 0 || r.h == 0 {
                return None;
            }
            let mut rect = snap::Rect {
                x: r.x,
                y: r.y,
                w: r.w as i32,
                h: r.h as i32,
            };
            if let Some((id, x, y, _)) = pending
                && id == d.id
            {
                rect = rect.at((x, y));
            }
            Some(Tile {
                id: d.id,
                name: display_name(tk, d),
                size: size(tk, &d.mode),
                rect,
                pc: d.kind == DisplayKind::Physical,
                shown: d.kind != DisplayKind::Physical || d.shown,
                main: s.primary == Some(d.id),
            })
        })
        .collect()
}

fn monitors(ui: &mut Ui, v: &mut View, s: &State, _revision: u32) {
    let tk = v.tk;
    let p = tk.p;
    header(ui, v, Page::Monitors);
    let online = s.connected().is_some();
    let hint = if v.ui.arc_view {
        "mon-hint-arc"
    } else {
        "mon-hint-flat"
    };
    let views = [tk.t("mon-flat"), tk.t("mon-arc")];
    let views: Vec<&str> = views.iter().map(String::as_str).collect();
    if let Some(i) = w::segmented(ui, tk, &views, v.ui.arc_view as usize, true) {
        v.ui.arc_view = i == 1;
    }
    ui.add_space(tk.th.shape.spacing * 1.5);

    let tiles = tiles(tk, s, v.ui.pending_place);
    // The map takes a good part of the window, and leaves the controls below it in view
    // in the smallest one.
    let height = (ui.ctx().content_rect().height() * 0.36).clamp(200.0, 420.0);
    if tiles.is_empty() {
        w::card(ui, tk, |ui| {
            text(ui, tk.t("mon-empty"), tk.body(), p.text_muted);
        });
    } else {
        let events = if v.ui.arc_view {
            map::arc(ui, tk, &mut v.ui.map, &tiles, v.ui.selected, height)
        } else {
            map::flat(ui, tk, &mut v.ui.map, &tiles, v.ui.selected, height)
        };
        for e in events {
            match e {
                MapEvent::Select(id) => v.ui.selected = Some(id),
                MapEvent::Place { id, x, y } => v.send(Request::Place { id, x, y }),
            }
        }
        ui.add_space(tk.th.shape.spacing);
        text(ui, tk.t(hint), tk.small(), p.text_muted);
    }
    ui.add_space(tk.th.shape.section_gap);

    let virtual_count = s
        .displays
        .iter()
        .filter(|d| d.kind == DisplayKind::Virtual)
        .count() as u32;
    let full = s.limits.displays_max.is_some_and(|m| virtual_count >= m);
    ui.horizontal_top(|ui| {
        let total = ui.available_width();
        let gap = tk.th.shape.padding;
        let left = (total - gap) * 0.55;
        ui.allocate_ui_with_layout(vec2(left, 0.0), Layout::top_down(Align::Min), |ui| {
            w::section_title(ui, tk, &tk.t("mon-selected"));
            w::card(ui, tk, |ui| selected_monitor(ui, v, s));
        });
        ui.add_space(gap - tk.th.shape.spacing);
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), 0.0),
            Layout::top_down(Align::Min),
            |ui| {
                w::section_title(ui, tk, &tk.t("mon-add"));
                w::card(ui, tk, |ui| {
                    let note = if !online {
                        tk.t("mon-add-offline")
                    } else if full {
                        tk.f("mon-add-full", &[("max", virtual_count.into())])
                    } else {
                        tk.t("mon-add-desc")
                    };
                    text(ui, note, tk.small(), p.text_muted);
                    ui.add_space(tk.th.shape.spacing * 1.5);
                    let sizes = if s.limits.sizes.is_empty() {
                        &SIZES[..]
                    } else {
                        &s.limits.sizes[..]
                    };
                    ui.horizontal_wrapped(|ui| {
                        for [wd, ht] in sizes {
                            let label =
                                tk.f("mon-size", &[("w", (*wd).into()), ("h", (*ht).into())]);
                            if w::button(ui, tk, &label, Kind::Secondary, online && !full).clicked()
                            {
                                v.send(Request::AddMonitor {
                                    w: *wd,
                                    h: *ht,
                                    scale: 100,
                                });
                            }
                        }
                    });
                });
            },
        );
    });
}

fn selected_monitor(ui: &mut Ui, v: &mut View, s: &State) {
    let tk = v.tk;
    let p = tk.p;
    let Some(d) = v.ui.selected.and_then(|id| s.display(id)) else {
        text(ui, tk.t("mon-none"), tk.body(), p.text_muted);
        return;
    };
    let mut name = display_name(tk, d);
    if d.kind == DisplayKind::Physical && !d.output.is_empty() && d.output != name {
        name = format!("{name} ({})", d.output);
    }
    text(ui, name, tk.heading(), p.text);
    let line = tk.f(
        "mon-size-scaled",
        &[
            ("w", d.mode.w.into()),
            ("h", d.mode.h.into()),
            ("scale", d.mode.scale.into()),
        ],
    );
    text(ui, line, tk.small(), p.text_muted);
    if let (DisplayState::Failed, Some(f)) = (d.state, &d.failure) {
        ui.add_space(tk.th.shape.spacing);
        w::status(
            ui,
            tk,
            p.danger,
            &tk.f("mon-failed", &[("reason", (&f.text).into())]),
        );
    }
    w::divider(ui, tk);
    let scales = if s.limits.scales.is_empty() {
        &SCALES[..]
    } else {
        &s.limits.scales[..]
    };
    let labels: Vec<String> = scales.iter().map(|sc| percent(tk, *sc)).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let current = scales
        .iter()
        .position(|sc| *sc == d.mode.scale)
        .unwrap_or(usize::MAX);
    w::row(ui, tk, &tk.t("mon-scale"), None, |ui| {
        if let Some(i) = w::segmented(ui, tk, &labels, current, true) {
            v.out.push(Action::Send(Request::SetScale {
                id: d.id,
                scale: scales[i],
            }));
        }
    });
    w::divider(ui, tk);
    if d.kind == DisplayKind::Physical {
        let opts = [tk.t("mon-shown"), tk.t("mon-hidden")];
        let opts: Vec<&str> = opts.iter().map(String::as_str).collect();
        w::row(ui, tk, &tk.t("mon-in-headset"), None, |ui| {
            if let Some(i) = w::segmented(ui, tk, &opts, (!d.shown) as usize, true) {
                v.out.push(Action::Send(Request::ShowPhysical {
                    id: d.id,
                    shown: i == 0,
                }));
            }
        });
        w::divider(ui, tk);
    }
    let main = s.primary == Some(d.id);
    let label = tk.t("mon-main");
    w::row(ui, tk, &label, Some(&tk.t("mon-main-desc")), |ui| {
        // The main one cannot be switched off: another monitor is made main instead.
        if w::switch(ui, tk, &label, main, !main) == Some(true) {
            v.out.push(Action::Send(Request::SetPrimary { id: d.id }));
        }
    });
    if d.kind == DisplayKind::Virtual {
        ui.add_space(tk.th.shape.padding * 0.75);
        ui.horizontal(|ui| {
            if d.state == DisplayState::Failed
                && w::button(ui, tk, &tk.t("ov-retry"), Kind::Primary, true).clicked()
            {
                v.out.push(Action::Send(Request::RetryMonitor { id: d.id }));
            }
            if w::button(ui, tk, &tk.t("mon-remove"), Kind::Danger, true).clicked() {
                v.out
                    .push(Action::Send(Request::RemoveMonitor { id: d.id }));
            }
        });
    }
}

// ------------------------------------------------------------------------- headsets

fn countdown(left: f64) -> String {
    let s = left.max(0.0).ceil() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// The pairing window's content: the code to type here or there, or what to do first.
/// Shared by the Headsets page and the first run.
fn pair_body(ui: &mut Ui, v: &mut View, pr: &Pairing, revision: u32) {
    let tk = v.tk;
    let p = tk.p;
    if let Some(code) = &pr.pc_code {
        text(ui, tk.t("pair-pc-code"), tk.body(), p.text_muted);
        ui.add_space(tk.th.shape.spacing);
        w::code_display(ui, tk, code);
    } else if let Some(name) = &pr.code_for {
        text(
            ui,
            tk.f("pair-type", &[("name", name.into())]),
            tk.body(),
            p.text_muted,
        );
        ui.add_space(tk.th.shape.spacing);
        let checking =
            v.ui.submitted
                .as_ref()
                .is_some_and(|(c, rev)| *rev == revision && *c == v.ui.code);
        let label = tk.t("pair-code");
        let enter = w::code_field(ui, tk, &label, &mut v.ui.code, !checking);
        let wrong = pr.last == Some(PairResult::Wrong)
            && v.ui
                .submitted
                .as_ref()
                .is_some_and(|(c, _)| *c == v.ui.code);
        if wrong && !checking {
            ui.add_space(tk.th.shape.spacing);
            text(ui, tk.t("pair-wrong"), tk.body(), p.danger);
        }
        ui.add_space(tk.th.shape.padding);
        ui.horizontal(|ui| {
            if checking {
                w::spinner(ui, tk, 22.0);
                text(ui, tk.t("pair-checking"), tk.body(), p.text);
            } else {
                let ready = v.ui.code.len() == 6 && !wrong;
                if (w::button(ui, tk, &tk.t("pair-go"), Kind::Primary, ready).clicked()
                    || (enter && ready))
                    && ready
                {
                    v.out.push(Action::Send(Request::PairingCode {
                        code: v.ui.code.clone(),
                    }));
                }
            }
        });
    } else {
        text(ui, tk.t("pair-wait"), tk.body(), p.text_muted);
        ui.add_space(tk.th.shape.padding);
        ui.horizontal(|ui| {
            w::spinner(ui, tk, 22.0);
            text(ui, tk.t("pair-waiting"), tk.body(), p.text);
        });
        ui.add_space(tk.th.shape.spacing);
        text(ui, tk.t("pair-firewall"), tk.small(), p.text_muted);
    }
}

fn pairing_card(ui: &mut Ui, v: &mut View, pr: &Pairing, revision: u32) {
    let tk = v.tk;
    let p = tk.p;
    let left = (v.ui.pair.ends - v.now).max(0.0);
    let total = pr.seconds_total.max(1) as f64;
    w::card(ui, tk, |ui| {
        ui.horizontal(|ui| {
            text(ui, tk.t("pair-title"), tk.heading(), p.text);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                text(
                    ui,
                    tk.f("pair-left", &[("time", countdown(left).into())]),
                    tk.body_m(),
                    p.text_muted,
                );
            });
        });
        ui.add_space(tk.th.shape.spacing);
        // The time left as a line that shrinks: pairing closes by itself.
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 3.0), Sense::hover());
        ui.painter()
            .rect_filled(r, CornerRadius::same(2), p.control);
        let mut f = r;
        f.set_right(r.left() + r.width() * (left / total).clamp(0.0, 1.0) as f32);
        ui.painter()
            .rect_filled(f, CornerRadius::same(2), p.accent_ink);
        ui.add_space(tk.th.shape.padding);
        pair_body(ui, v, pr, revision);
        ui.add_space(tk.th.shape.spacing);
        if w::button(ui, tk, &tk.t("pair-stop"), Kind::Quiet, true).clicked() {
            v.send(Request::StopPairing);
        }
    });
}

fn outcome_card(ui: &mut Ui, v: &mut View, o: &Outcome) {
    let tk = v.tk;
    let p = tk.p;
    w::card(ui, tk, |ui| {
        match o {
            Outcome::Paired(name) => {
                w::status(ui, tk, p.ok, &tk.f("pair-done", &[("name", name.into())]));
            }
            Outcome::Expired => {
                text(ui, tk.t("pair-expired"), tk.body(), p.text_muted);
            }
        }
        ui.add_space(tk.th.shape.padding);
        ui.horizontal(|ui| {
            if *o == Outcome::Expired
                && w::button(ui, tk, &tk.t("pair-reopen"), Kind::Primary, true).clicked()
            {
                v.send(Request::PairNewHeadset);
            }
            if w::button(ui, tk, &tk.t("pair-close"), Kind::Quiet, true).clicked() {
                v.ui.pair.outcome = None;
            }
        });
    });
}

fn headsets(ui: &mut Ui, v: &mut View, s: &State) {
    let tk = v.tk;
    let p = tk.p;
    header(ui, v, Page::Headsets);
    let revision = match v.conn {
        Conn::Live { snap, .. } => snap.revision,
        _ => 0,
    };
    let gap = tk.th.shape.section_gap;
    if let Some(pr) = &s.pairing {
        pairing_card(ui, v, pr, revision);
        ui.add_space(gap);
    } else if let Some(o) = v.ui.pair.outcome.clone() {
        outcome_card(ui, v, &o);
        ui.add_space(gap);
    } else if !s.headsets.is_empty() {
        if w::button(ui, tk, &tk.t("ov-pair-new"), Kind::Primary, true).clicked() {
            v.send(Request::PairNewHeadset);
        }
        ui.add_space(gap);
    }
    if s.headsets.is_empty() {
        if s.pairing.is_none() {
            w::card(ui, tk, |ui| {
                text(ui, tk.t("hs-empty-title"), tk.heading(), p.text);
                ui.add_space(6.0);
                text(ui, tk.t("hs-empty-body"), tk.body(), p.text_muted);
                ui.add_space(tk.th.shape.padding);
                if w::button(ui, tk, &tk.t("ov-pair-new"), Kind::Primary, true).clicked() {
                    v.send(Request::PairNewHeadset);
                }
            });
        }
        return;
    }
    let n = s.headsets.len();
    w::card(ui, tk, |ui| {
        for (i, h) in s.headsets.iter().enumerate() {
            if v.ui.forget_ask.as_deref() == Some(h.key.as_str()) {
                text(
                    ui,
                    tk.f("hs-forget-ask", &[("name", (&h.name).into())]),
                    tk.body_m(),
                    p.text,
                );
                ui.add_space(tk.th.shape.spacing);
                ui.horizontal(|ui| {
                    let named = tk.f("hs-forget-named", &[("name", (&h.name).into())]);
                    if w::button_named(ui, tk, &tk.t("hs-forget"), &named, Kind::Danger, true)
                        .clicked()
                    {
                        v.out.push(Action::Send(Request::ForgetHeadset {
                            headset: h.key.clone(),
                        }));
                        v.ui.forget_ask = None;
                    }
                    if w::button(ui, tk, &tk.t("hs-cancel"), Kind::Quiet, true).clicked() {
                        v.ui.forget_ask = None;
                    }
                });
            } else {
                let full = ui.available_width();
                ui.allocate_ui_with_layout(
                    vec2(full, 0.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        let named = tk.f("hs-forget-named", &[("name", (&h.name).into())]);
                        if w::button_named(ui, tk, &tk.t("hs-forget"), &named, Kind::Danger, true)
                            .clicked()
                        {
                            v.ui.forget_ask = Some(h.key.clone());
                        }
                        ui.add_space(tk.th.shape.padding);
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            text(ui, &h.name, tk.body_m(), p.text);
                            ui.add_space(2.0);
                            if h.session.is_some() {
                                ui.horizontal(|ui| {
                                    let (r, _) =
                                        ui.allocate_exact_size(vec2(10.0, 16.0), Sense::hover());
                                    w::dot(ui, tk, r.center(), p.ok);
                                    text(ui, tk.t("hs-now"), tk.small(), p.text_muted);
                                });
                            } else {
                                let when = match h.last_seen {
                                    Some(t) => {
                                        tk.f("hs-last", &[("when", ago(tk, v.now_unix, t).into())])
                                    }
                                    None => tk.t("hs-never"),
                                };
                                text(ui, when, tk.small(), p.text_muted);
                            }
                        });
                    },
                );
            }
            if i + 1 < n {
                w::divider(ui, tk);
            }
        }
    });
}

// ------------------------------------------------------------------------- sound

fn sound(ui: &mut Ui, v: &mut View, s: &State) {
    let tk = v.tk;
    let p = tk.p;
    header(ui, v, Page::Sound);
    let connected = s.connected();
    w::card(ui, tk, |ui| {
        let opts = [tk.t("sound-in-headset"), tk.t("sound-on-pc")];
        let opts: Vec<&str> = opts.iter().map(String::as_str).collect();
        w::row(
            ui,
            tk,
            &tk.t("snd-where"),
            Some(&tk.t("snd-where-desc")),
            |ui| {
                if let Some(i) = w::segmented(ui, tk, &opts, (!s.sound.to_headset) as usize, true) {
                    v.out
                        .push(Action::Send(Request::SoundToHeadset { on: i == 0 }));
                }
            },
        );
        w::divider(ui, tk);
        let vol = s.sound.volume;
        let desc = if vol.is_some() {
            tk.t("snd-volume-desc")
        } else {
            tk.t("snd-volume-off")
        };
        let label = tk.t("snd-volume");
        w::row(ui, tk, &label, Some(&desc), |ui| {
            let level = v.ui.volume_drag.or(vol.map(|v| v.level)).unwrap_or(0);
            ui.allocate_ui_with_layout(
                vec2(56.0, tk.th.shape.control_height),
                Layout::right_to_left(Align::Center),
                |ui| {
                    let c = if vol.is_some() { p.text } else { p.text_muted };
                    text(ui, percent(tk, level), tk.body_m(), c);
                },
            );
            if let Some((lv, done)) = w::slider(ui, tk, &label, level, 240.0, vol.is_some()) {
                if done {
                    v.ui.volume_drag = None;
                    v.out.push(Action::Send(Request::Volume { level: lv }));
                } else {
                    v.ui.volume_drag = Some(lv);
                }
            }
        });
    });
    ui.add_space(tk.th.shape.section_gap);
    w::card(ui, tk, |ui| {
        let label = tk.t("snd-mic");
        w::row(ui, tk, &label, Some(&tk.t("snd-mic-desc")), |ui| {
            if let Some(on) = w::switch(ui, tk, &label, s.sound.mic_on, true) {
                v.out.push(Action::Send(Request::SetMic { on }));
            }
        });
        w::divider(ui, tk);
        match connected {
            None if s.sound.mic_on => w::status(ui, tk, p.text_muted, &tk.t("snd-mic-waiting")),
            Some((_, session)) if s.sound.mic_on && session.mic_allowed == Some(false) => {
                w::status(ui, tk, p.warning, &tk.t("snd-mic-not-allowed"))
            }
            _ => match (s.sound.mic_on, s.sound.mic) {
                (false, _) => w::status(ui, tk, p.text_muted, &tk.t("snd-mic-off")),
                (true, MicState::Failed) => {
                    let why = s.sound.mic_failure.as_ref().map_or("", |f| f.text.as_str());
                    w::status(
                        ui,
                        tk,
                        p.danger,
                        &tk.f("snd-mic-failed", &[("reason", why.into())]),
                    )
                }
                (true, MicState::Ready) => w::status(ui, tk, p.ok, &tk.t("snd-mic-ready")),
                (true, _) => w::status(ui, tk, p.text_muted, &tk.t("snd-mic-waiting")),
            },
        }
    });
}

// ------------------------------------------------------------------------- look

fn look(ui: &mut Ui, v: &mut View) {
    let tk = v.tk;
    let p = tk.p;
    header(ui, v, Page::Look);
    let live = matches!(v.conn, Conn::Live { .. });
    let current = v.look.clone();
    w::card(ui, tk, |ui| {
        let modes = [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark];
        let labels = [tk.t("look-system"), tk.t("look-light"), tk.t("look-dark")];
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let now = (current.theme == ThemeMode::System).then(|| match v.system {
            Some(Scheme::Dark) => tk.t("look-now-dark"),
            Some(Scheme::Light) => tk.t("look-now-light"),
            None => tk.t("look-now-none"),
        });
        let sel = modes.iter().position(|m| *m == current.theme).unwrap_or(0);
        w::row(ui, tk, &tk.t("look-theme"), now.as_deref(), |ui| {
            if let Some(i) = w::segmented(ui, tk, &labels, sel, true) {
                v.out.push(Action::Look(WindowLook {
                    theme: modes[i],
                    ..current.clone()
                }));
            }
        });
        w::divider(ui, tk);
        let labels = [tk.t("look-palette-ink"), tk.t("look-palette-graphite")];
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let sel = PaletteName::ALL
            .iter()
            .position(|p| p.id() == current.palette)
            .unwrap_or(0);
        w::row(
            ui,
            tk,
            &tk.t("look-palette"),
            Some(&tk.t("look-palette-desc")),
            |ui| {
                if let Some(i) = w::segmented(ui, tk, &labels, sel, true) {
                    v.out.push(Action::Look(WindowLook {
                        palette: PaletteName::ALL[i].id().to_owned(),
                        ..current.clone()
                    }));
                }
            },
        );
        w::divider(ui, tk);
        let code = tk.s.code().to_owned();
        let more = v.lang_dirs.first().map(|d| {
            tk.f(
                "look-lang-more",
                &[("dir", prostir_lang::short_path(d).into())],
            )
        });
        w::row(ui, tk, &tk.t("look-lang"), more.as_deref(), |ui| {
            // Each language is named in itself.
            let names: Vec<&str> = v.languages.iter().map(|l| l.name.as_str()).collect();
            let sel = v.languages.iter().position(|l| l.code == code).unwrap_or(0);
            let picked = if names.len() <= 4 {
                w::segmented(ui, tk, &names, sel, true)
            } else {
                let mut i = sel;
                egui::ComboBox::from_id_salt("language")
                    .selected_text(names.get(sel).copied().unwrap_or(""))
                    .show_index(ui, &mut i, names.len(), |k| names[k]);
                (i != sel).then_some(i)
            };
            if let Some(i) = picked {
                v.out.push(Action::Look(WindowLook {
                    language: Some(v.languages[i].code.clone()),
                    ..current.clone()
                }));
            }
        });
        if !live {
            w::divider(ui, tk);
            w::status(ui, tk, p.text_muted, &tk.t("look-local"));
        }
    });

    let Some(path) = v.theme_file else { return };
    ui.add_space(tk.th.shape.section_gap);
    w::section_title(ui, tk, &tk.t("look-file"));
    w::card(ui, tk, |ui| {
        let file = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir = path.parent().unwrap_or(path).to_path_buf();
        let shown_dir = prostir_lang::short_path(&dir);
        w::row(ui, tk, &file, Some(&shown_dir), |ui| {
            if w::button(ui, tk, &tk.t("look-open"), Kind::Secondary, true).clicked() {
                v.out.push(Action::OpenFolder(dir.clone()));
            }
            if !v.theme_file_exists
                && w::button(ui, tk, &tk.t("look-create"), Kind::Secondary, true).clicked()
            {
                v.out.push(Action::CreateThemeFile(path.to_path_buf()));
            }
        });
        w::divider(ui, tk);
        let th = tk.th;
        if !v.theme_file_exists {
            w::status(ui, tk, p.text_muted, &tk.t("look-file-none"));
        } else if th.issues.is_empty() && th.contrast.is_empty() {
            w::status(ui, tk, p.ok, &tk.t("look-file-ok"));
        }
        if !th.issues.is_empty() {
            w::status(ui, tk, p.warning, &tk.t("look-file-issues"));
            for i in &th.issues {
                let line = match &i.kind {
                    IssueKind::NotToml(why) => tk.f("issue-not-toml", &[("why", why.into())]),
                    IssueKind::Format(value) => tk.f("issue-format", &[("value", value.into())]),
                    IssueKind::BadValue(value) => tk.f(
                        "issue-bad",
                        &[("at", (&i.at).into()), ("value", value.into())],
                    ),
                    IssueKind::Unknown => tk.f("issue-unknown", &[("at", (&i.at).into())]),
                    IssueKind::Font(why) => {
                        tk.f("issue-font", &[("at", (&i.at).into()), ("why", why.into())])
                    }
                    IssueKind::Unreadable(why) => tk.f("issue-unreadable", &[("why", why.into())]),
                };
                indented(ui, tk, line);
            }
        }
        if !th.contrast.is_empty() {
            ui.add_space(tk.th.shape.spacing);
            w::status(ui, tk, p.warning, &tk.t("look-file-contrast"));
            for c in &th.contrast {
                let scheme = match c.scheme {
                    Scheme::Light => tk.t("scheme-light"),
                    Scheme::Dark => tk.t("scheme-dark"),
                };
                let line = tk.f(
                    "contrast-line",
                    &[
                        ("scheme", scheme.into()),
                        ("fg", c.fg.into()),
                        ("bg", c.bg.into()),
                        ("ratio", decimal(tk, &format!("{:.1}", c.ratio)).into()),
                        ("need", decimal(tk, &format!("{}", c.need)).into()),
                    ],
                );
                indented(ui, tk, line);
            }
        }
    });
}

/// A number written with a point, with the language's decimal separator.
fn decimal(tk: &Tk, n: &str) -> String {
    n.replace('.', &tk.t("decimal-separator"))
}

fn indented(ui: &mut Ui, tk: &Tk, line: String) {
    ui.horizontal(|ui| {
        ui.add_space(tk.th.shape.padding);
        text(ui, line, tk.small(), tk.p.text);
    });
}

// ------------------------------------------------------------------------- first run

const STEPS: usize = 4;

fn first_run(ui: &mut Ui, v: &mut View, s: &State) {
    let tk = v.tk;
    let p = tk.p;
    header(ui, v, Page::FirstRun);
    let step = v.ui.fr_step.min(STEPS - 1);
    text(
        ui,
        tk.f(
            "fr-step",
            &[("n", (step + 1).into()), ("total", STEPS.into())],
        ),
        tk.small(),
        p.text_muted,
    );
    ui.add_space(tk.th.shape.spacing);
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    let gap = tk.th.shape.spacing * 0.75;
    let seg = (r.width() - gap * (STEPS - 1) as f32) / STEPS as f32;
    for i in 0..STEPS {
        let piece = Rect::from_min_size(
            pos2(r.left() + i as f32 * (seg + gap), r.top()),
            vec2(seg, 4.0),
        );
        let c = if i <= step { p.accent_ink } else { p.control };
        ui.painter().rect_filled(piece, CornerRadius::same(2), c);
    }
    ui.add_space(tk.th.shape.section_gap * 0.75);

    let title = |ui: &mut Ui, id: &str| {
        text(ui, tk.t(id), tk.font(tk.th.type_.size_title, true), p.text);
        ui.add_space(tk.th.shape.spacing);
    };
    let revision = match v.conn {
        Conn::Live { snap, .. } => snap.revision,
        _ => 0,
    };
    let paired = match &v.ui.pair.outcome {
        Some(Outcome::Paired(name)) => Some(name.clone()),
        _ => None,
    };
    let mut next_ok = true;
    w::card(ui, tk, |ui| {
        ui.add_space(tk.th.shape.spacing);
        match step {
            0 => {
                w::brand_mark(ui, tk, 72.0);
                ui.add_space(tk.th.shape.padding);
                title(ui, "fr-1-title");
                text(ui, tk.t("fr-1-body"), tk.body(), p.text_muted);
                ui.add_space(tk.th.shape.padding);
                if w::button(ui, tk, &tk.t("fr-1-link"), Kind::Secondary, true).clicked() {
                    v.out.push(Action::OpenUrl(RELEASES.into()));
                }
            }
            1 => {
                title(ui, "fr-2-title");
                text(ui, tk.t("fr-2-body"), tk.body(), p.text_muted);
                ui.add_space(tk.th.shape.padding);
                let found = s
                    .nearby
                    .first()
                    .or(s.pairing.as_ref().and_then(|p| p.code_for.as_ref()));
                match found {
                    Some(name) => {
                        w::status(ui, tk, p.ok, &tk.f("fr-2-found", &[("name", name.into())]))
                    }
                    None if s.pairing.is_none() => {
                        w::status(ui, tk, p.text_muted, &tk.t("fr-2-closed"));
                        ui.add_space(tk.th.shape.spacing);
                        if w::button(ui, tk, &tk.t("pair-reopen"), Kind::Secondary, true).clicked()
                        {
                            v.send(Request::PairNewHeadset);
                        }
                    }
                    // Next stays open: a headset reached by its address or over USB
                    // is not "seen" first, it asks for the code straight away.
                    None => {
                        ui.horizontal(|ui| {
                            w::spinner(ui, tk, 22.0);
                            text(ui, tk.t("fr-2-looking"), tk.body(), p.text);
                        });
                    }
                }
                ui.add_space(tk.th.shape.padding);
                text(ui, tk.t("pair-firewall"), tk.small(), p.text_muted);
            }
            2 => {
                title(ui, "fr-3-title");
                text(ui, tk.t("fr-3-body"), tk.body(), p.text_muted);
                ui.add_space(tk.th.shape.padding);
                if let Some(name) = &paired {
                    w::status(ui, tk, p.ok, &tk.f("pair-done", &[("name", name.into())]));
                } else if let Some(pr) = &s.pairing {
                    pair_body(ui, v, pr, revision);
                    next_ok = false;
                } else {
                    text(ui, tk.t("pair-expired"), tk.body(), p.text_muted);
                    ui.add_space(tk.th.shape.padding);
                    if w::button(ui, tk, &tk.t("pair-reopen"), Kind::Primary, true).clicked() {
                        v.send(Request::PairNewHeadset);
                    }
                    next_ok = !s.headsets.is_empty();
                }
            }
            _ => {
                title(ui, "fr-4-title");
                text(ui, tk.t("fr-4-body"), tk.body(), p.text_muted);
                if let Some(on) = s.autostart {
                    ui.add_space(tk.th.shape.padding);
                    let label = tk.t("ov-autostart");
                    w::row(ui, tk, &label, None, |ui| {
                        if let Some(on) = w::switch(ui, tk, &label, on, true) {
                            v.out.push(Action::Send(Request::Autostart { on }));
                        }
                    });
                }
            }
        }
        ui.add_space(tk.th.shape.section_gap);
        let last = step + 1 == STEPS;
        let label = if last {
            tk.t("fr-done")
        } else {
            tk.t("fr-next")
        };
        let back = tk.t("fr-back");
        // Laid out left to right after a gap, so Tab meets Back before Next, as seen.
        let width = w::button_width(ui, tk, &label)
            + if step > 0 {
                w::button_width(ui, tk, &back) + ui.spacing().item_spacing.x
            } else {
                0.0
            };
        ui.horizontal(|ui| {
            ui.add_space((ui.available_width() - width).max(0.0));
            if step > 0 && w::button(ui, tk, &back, Kind::Quiet, true).clicked() {
                v.ui.fr_step = step - 1;
            }
            {
                if w::button(ui, tk, &label, Kind::Primary, next_ok).clicked() {
                    if last {
                        v.ui.fr_step = 0;
                        v.ui.pair.outcome = None;
                        v.out.push(Action::Go(Page::Overview));
                    } else {
                        // The headset can only find the PC while pairing is open.
                        if step == 0 && s.pairing.is_none() {
                            v.out.push(Action::Send(Request::PairNewHeadset));
                        }
                        v.ui.fr_step = step + 1;
                    }
                }
            }
        });
    });
}
