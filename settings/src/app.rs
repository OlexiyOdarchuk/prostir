//! The window: the sidebar, the page shown, and everything between the pages and the
//! service. The window keeps no settings of its own: it shows the service's state and
//! asks the service for every change. What is its own is how it looks right now (the
//! page, the selected monitor, a code being typed, the cross-fade).

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Context, FontFamily, FontId, Layout, Sense, Stroke, TextStyle, Ui, Visuals, vec2,
};
use prostir_lang::{Catalog, Language, Strings};

use crate::api::*;
use crate::instance::Presented;
use crate::link::{Link, Update};
use crate::map::MapState;
use crate::pages::{self, Action, View};
use crate::portal::SystemScheme;
use crate::theme::{Palette, PaletteName, Scheme, Theme, ThemeFile};
use crate::widgets::{self as w, Kind, Tk, cr, text};

/// The settings window's translations, built in.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("en", include_str!("../lang/en.ftl")),
    ("uk", include_str!("../lang/uk.ftl")),
];

/// The folder name under `prostir/lang/` for this window's translations.
pub const LANG_DOMAIN: &str = "settings";

/// How long "Starting Prostir" waits for the service to appear.
const START_WAIT: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Overview,
    Monitors,
    Headsets,
    Sound,
    Look,
    FirstRun,
}

/// Starts the service ("Start Prostir"); an error means it could not even be run.
pub type Starter = Box<dyn FnMut() -> std::io::Result<()>>;

/// Runs `prostir` on its own, detached from the window.
pub fn start_prostir() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("prostir")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()?;
    // Reaped when it ends, so it never lingers as a zombie of the window.
    std::thread::spawn(move || child.wait());
    Ok(())
}

pub struct Options {
    pub client: Box<dyn Client>,
    /// The client on a thread of its own (the window); inline for tests.
    pub threaded: bool,
    /// The person's theme file; none: only the built-in themes.
    pub theme_file: Option<PathBuf>,
    /// Folders with more translations; none: the XDG data folders.
    pub lang_dirs: Option<Vec<PathBuf>>,
    /// The desktop's language; none: read from the environment (`LANG` and so on).
    pub language: Option<String>,
    /// The desktop's light or dark preference, followed elsewhere.
    pub system: SystemScheme,
    pub start: Starter,
    pub presented: Option<Presented>,
}

/// How starting the service from the window went.
#[derive(Clone, Debug, PartialEq)]
pub enum Start {
    Idle,
    Waiting { since: f64 },
    NoProgram,
    TimedOut,
}

/// The line to the service.
#[derive(Clone, Debug, PartialEq)]
pub enum Conn {
    /// Before the first answer.
    Loading,
    Absent {
        why: ClientError,
        start: Start,
    },
    Live {
        snap: Snapshot,
        at: f64,
    },
}

/// How a pairing window ended, while the window shows it.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Paired(String),
    Expired,
}

/// The pairing window as the window follows it across states.
#[derive(Clone, Debug, Default)]
pub struct PairWatch {
    pub open: bool,
    keys: Vec<String>,
    /// When it closes, in the window's time.
    pub ends: f64,
    pub outcome: Option<Outcome>,
}

/// What is the window's own: what it shows and what is being typed.
pub struct UiState {
    pub page: Page,
    pub map: MapState,
    pub arc_view: bool,
    pub selected: Option<u32>,
    /// A monitor moved here, shown so until a newer state says where it is.
    pub pending_place: Option<(u32, i32, i32, u32)>,
    /// The volume while its slider is held.
    pub volume_drag: Option<u8>,
    pub forget_ask: Option<String>,
    pub code: String,
    /// The code sent, and the state's revision when it was.
    pub submitted: Option<(String, u32)>,
    pub pair: PairWatch,
    pub fr_step: usize,
    pub notice: Option<String>,
    /// The first state decided which page opens.
    opened: bool,
}

pub struct App {
    link: Link,
    pub conn: Conn,
    look: WindowLook,
    catalog: Catalog,
    pub languages: Vec<Language>,
    strings: Strings,
    desktop_language: Option<String>,
    file: ThemeFile,
    theme: Theme,
    palette: PaletteName,
    shown: Option<Palette>,
    fade: Option<(Palette, f64)>,
    scheme: Option<Scheme>,
    applied: Option<(Palette, Theme)>,
    fonts: Option<(String, String)>,
    system: SystemScheme,
    start: Starter,
    presented: Option<Presented>,
    pub ui: UiState,
}

impl App {
    pub fn new(ctx: &Context, o: Options) -> Self {
        let link = if o.threaded {
            let c = ctx.clone();
            Link::spawn(o.client, move || c.request_repaint())
        } else {
            Link::inline(o.client)
        };
        let catalog = match o.lang_dirs {
            Some(dirs) => Catalog::with_dirs(LANGUAGES, dirs),
            None => Catalog::new(LANG_DOMAIN, LANGUAGES),
        };
        let languages = catalog.languages();
        let look = WindowLook::default();
        let code = pick_language(&o.language, &look, &languages);
        let strings = catalog.load(&code);
        let file = ThemeFile::new(o.theme_file);
        let palette = PaletteName::Ink;
        let theme = Theme::load(palette, file.text());
        if let Some(p) = &o.presented {
            p.attach(ctx);
        }
        Self {
            link,
            conn: Conn::Loading,
            look,
            catalog,
            languages,
            strings,
            desktop_language: o.language,
            file,
            theme,
            palette,
            shown: None,
            fade: None,
            scheme: None,
            applied: None,
            fonts: None,
            system: o.system,
            start: o.start,
            presented: o.presented,
            ui: UiState {
                page: Page::Overview,
                map: MapState::default(),
                arc_view: false,
                selected: None,
                pending_place: None,
                volume_drag: None,
                forget_ask: None,
                code: String::new(),
                submitted: None,
                pair: PairWatch::default(),
                fr_step: 0,
                notice: None,
                opened: false,
            },
        }
    }

    /// The state, while the service is there.
    pub fn state(&self) -> Option<&State> {
        match &self.conn {
            Conn::Live { snap, .. } => Some(&snap.state),
            _ => None,
        }
    }

    pub fn look(&self) -> &WindowLook {
        &self.look
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    fn absorb(&mut self, u: Update, now: f64) {
        match u {
            Update::State(snap) => {
                let prev = self.state().cloned();
                if prev.as_ref().is_none_or(|p| p.window != snap.state.window) {
                    self.look = snap.state.window.clone();
                }
                self.watch_pairing(prev.as_ref(), &snap.state, now);
                if self
                    .ui
                    .pending_place
                    .is_some_and(|(.., rev)| snap.revision > rev)
                {
                    self.ui.pending_place = None;
                }
                if !self.ui.opened {
                    self.ui.opened = true;
                    if snap.state.headsets.is_empty() {
                        self.ui.page = Page::FirstRun;
                    }
                }
                if self
                    .ui
                    .selected
                    .is_some_and(|id| snap.state.display(id).is_none())
                {
                    self.ui.selected = None;
                }
                self.conn = Conn::Live { snap, at: now };
            }
            Update::Absent(why) => {
                let start = match &self.conn {
                    Conn::Absent { start, .. } => start.clone(),
                    _ => Start::Idle,
                };
                self.ui.pair = PairWatch::default();
                self.conn = Conn::Absent { why, start };
            }
            Update::Replied {
                request,
                result: Err(e),
            } => {
                // Not running is shown by the page itself; a look chosen without the
                // service still holds for this window.
                if !(matches!(request, Request::WindowLook { .. }) && e == ClientError::NotRunning)
                {
                    self.ui.notice = Some(self.error_text(&e));
                }
                if matches!(request, Request::Place { .. }) {
                    self.ui.pending_place = None;
                }
            }
            Update::Replied { .. } => {}
        }
    }

    fn error_text(&self, e: &ClientError) -> String {
        let s = &self.strings;
        match e {
            ClientError::Refused { message, code } => s.f(
                "notice-failed",
                &[(
                    "why",
                    if message.is_empty() { code } else { message }.into(),
                )],
            ),
            ClientError::TimedOut => s.t("notice-timeout"),
            ClientError::NotRunning | ClientError::NoBus(_) => s.t("notice-not-running"),
            ClientError::Broken(why) => s.f("notice-broken", &[("why", why.into())]),
        }
    }

    fn watch_pairing(&mut self, prev: Option<&State>, s: &State, now: f64) {
        let pw = &mut self.ui.pair;
        let keys = |st: &State| {
            st.headsets
                .iter()
                .map(|h| h.key.clone())
                .collect::<Vec<_>>()
        };
        match (&s.pairing, pw.open) {
            (Some(p), was) => {
                if !was {
                    pw.open = true;
                    pw.keys = prev.map_or_else(|| keys(s), keys);
                    pw.outcome = None;
                    self.ui.code.clear();
                    self.ui.submitted = None;
                }
                pw.ends = now + p.seconds_left as f64;
            }
            (None, true) => {
                pw.open = false;
                let new = s.headsets.iter().find(|h| !pw.keys.contains(&h.key));
                pw.outcome = match new {
                    Some(h) => Some(Outcome::Paired(h.name.clone())),
                    None if now >= pw.ends - 3.0 => Some(Outcome::Expired),
                    None => None,
                };
                self.ui.code.clear();
                self.ui.submitted = None;
            }
            (None, false) => {}
        }
    }

    fn act(&mut self, a: Action, ctx: &Context, now: f64) {
        match a {
            Action::Send(r) => {
                if let Request::Place { id, x, y } = r {
                    let rev = match &self.conn {
                        Conn::Live { snap, .. } => snap.revision,
                        _ => 0,
                    };
                    self.ui.pending_place = Some((id, x, y, rev));
                }
                if let Request::PairingCode { code } = &r {
                    let rev = match &self.conn {
                        Conn::Live { snap, .. } => snap.revision,
                        _ => 0,
                    };
                    self.ui.submitted = Some((code.clone(), rev));
                }
                self.link.send(r);
            }
            Action::Go(page) => {
                self.ui.page = page;
                self.ui.forget_ask = None;
            }
            Action::Look(look) => {
                self.look = look.clone();
                self.link.send(Request::WindowLook {
                    theme: look.theme,
                    palette: look.palette,
                    language: look.language,
                });
            }
            Action::Start => {
                let start = match (self.start)() {
                    Ok(()) => Start::Waiting { since: now },
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Start::NoProgram,
                    Err(e) => {
                        log::warn!("could not start prostir: {e}");
                        Start::TimedOut
                    }
                };
                if let Conn::Absent { start: s, .. } = &mut self.conn {
                    *s = start;
                }
                self.link.refresh();
            }
            Action::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Action::OpenFolder(dir) => {
                let _ = std::fs::create_dir_all(&dir);
                if let Err(e) = std::process::Command::new("xdg-open").arg(&dir).spawn() {
                    log::warn!("could not open {}: {e}", dir.display());
                }
            }
            Action::CreateThemeFile(path) => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                // Never over a file that is there: it may have been written by hand.
                let made = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .and_then(|mut f| {
                        std::io::Write::write_all(&mut f, crate::theme::INK.as_bytes())
                    });
                if let Err(e) = made {
                    self.ui.notice = Some(
                        self.strings
                            .f("notice-failed", &[("why", e.to_string().into())]),
                    );
                }
            }
            Action::Dismiss => self.ui.notice = None,
        }
    }

    /// The language now: chosen in the window, or the desktop's.
    fn language(&self) -> String {
        pick_language(&self.desktop_language, &self.look, &self.languages)
    }

    fn scheme_now(&self, ctx: &Context) -> Scheme {
        match self.look.theme {
            ThemeMode::Light => Scheme::Light,
            ThemeMode::Dark => Scheme::Dark,
            ThemeMode::System => self
                .system
                .get()
                .or_else(|| {
                    ctx.system_theme().map(|t| match t {
                        egui::Theme::Dark => Scheme::Dark,
                        egui::Theme::Light => Scheme::Light,
                    })
                })
                .unwrap_or(Scheme::Dark),
        }
    }

    /// Theme, fonts and the colours shown now, cross-fading between light and dark.
    fn prepare_look(&mut self, ctx: &Context, now: f64) {
        let palette = PaletteName::from_id(&self.look.palette).unwrap_or_default();
        if self.file.changed(now) || palette != self.palette {
            self.palette = palette;
            self.theme = Theme::load(palette, self.file.text());
            if let Some(shown) = &self.shown {
                self.fade = Some((shown.clone(), now));
            }
        }
        let scheme = self.scheme_now(ctx);
        let target = self.theme.palette(scheme).clone();
        if self.scheme.is_some_and(|s| s != scheme)
            && let Some(shown) = &self.shown
        {
            self.fade = Some((shown.clone(), now));
        }
        self.scheme = Some(scheme);
        let theme_ms = self.theme.motion.theme_ms;
        self.shown = Some(match self.fade.take() {
            Some((from, start)) if theme_ms > 0.0 => {
                let t = ((now - start) as f32 / (theme_ms / 1000.0)).min(1.0);
                if t < 1.0 {
                    ctx.request_repaint();
                    self.fade = Some((from.clone(), start));
                    from.lerp(&target, t * t * (3.0 - 2.0 * t))
                } else {
                    target
                }
            }
            _ => target,
        });
        let fonts = (
            self.theme.type_.font.clone(),
            self.theme.type_.font_medium.clone(),
        );
        if self.fonts.as_ref() != Some(&fonts) {
            ctx.set_fonts(crate::fonts::definitions(&self.theme.type_));
            self.fonts = Some(fonts);
        }
        let shown = self.shown.clone().expect("set above");
        if self
            .applied
            .as_ref()
            .is_none_or(|(p, t)| *p != shown || *t != self.theme)
        {
            apply_style(ctx, &shown, &self.theme, scheme);
            self.applied = Some((shown, self.theme.clone()));
        }
        let code = self.language();
        if code != self.strings.code() {
            self.strings = self.catalog.load(&code);
        }
    }

    pub fn show(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        for u in self.link.poll() {
            self.absorb(u, now);
        }
        if let Conn::Absent {
            start: start @ Start::Waiting { .. },
            ..
        } = &mut self.conn
            && let Start::Waiting { since } = *start
            && now - since > START_WAIT
        {
            *start = Start::TimedOut;
        }
        if self.presented.as_ref().is_some_and(Presented::take) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.prepare_look(&ctx, now);
        // The theme file is looked at once a second, and countdowns tick.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.ui.notice = None;
            self.ui.forget_ask = None;
        }

        let mut actions = Vec::new();
        {
            let shown = self.shown.as_ref().expect("prepared");
            let tk = Tk {
                p: shown,
                th: &self.theme,
                s: &self.strings,
            };
            let sh = &self.theme.shape;
            let first_run_shown = self.ui.page == Page::FirstRun
                || self.state().is_some_and(|s| s.headsets.is_empty());
            egui::Panel::left("pages")
                .exact_size(sh.sidebar_width)
                .resizable(false)
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(tk.p.sidebar)
                        .inner_margin(egui::Margin::symmetric(16, 20)),
                )
                .show(ui, |ui| {
                    sidebar(ui, &tk, &self.conn, &self.ui, first_run_shown, &mut actions)
                });

            let pad = |v: f32| v.round().clamp(0.0, 127.0) as i8;
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(tk.p.background)
                        .inner_margin(egui::Margin {
                            left: pad(sh.section_gap * 1.5),
                            right: pad(sh.padding),
                            top: pad(sh.section_gap * 1.25),
                            bottom: pad(sh.padding),
                        }),
                )
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            ui.set_max_width(ui.available_width().min(sh.content_max_width));
                            if let Some(n) = &self.ui.notice {
                                notice(ui, &tk, n, &mut actions);
                            }
                            let mut v = View {
                                tk: &tk,
                                conn: &self.conn,
                                ui: &mut self.ui,
                                out: &mut actions,
                                now,
                                now_unix: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map_or(0, |d| d.as_secs()),
                                look: &self.look,
                                languages: &self.languages,
                                lang_dirs: self.catalog.dirs(),
                                system: self.system.get(),
                                theme_file: self.file.path(),
                                theme_file_exists: self.file.exists(),
                            };
                            pages::show(ui, &mut v);
                            ui.add_space(sh.section_gap);
                        });
                });
        }
        for a in actions {
            self.act(a, &ctx, now);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

/// The language chosen in the window, else the desktop's, among those there are.
fn pick_language(desktop: &Option<String>, look: &WindowLook, langs: &[Language]) -> String {
    let codes: Vec<&str> = langs.iter().map(|l| l.code.as_str()).collect();
    let known = |c: &String| codes.contains(&c.as_str());
    look.language
        .clone()
        .filter(known)
        .or_else(|| desktop.clone().filter(known))
        .unwrap_or_else(|| prostir_lang::system_language(&codes))
}

/// egui's own look from the theme, for what egui draws itself (scroll bars, text
/// selection, the text cursor).
fn apply_style(ctx: &Context, p: &Palette, th: &Theme, scheme: Scheme) {
    let mut v = match scheme {
        Scheme::Dark => Visuals::dark(),
        Scheme::Light => Visuals::light(),
    };
    v.override_text_color = Some(p.text);
    v.panel_fill = p.background;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.well;
    v.faint_bg_color = p.control;
    v.text_edit_bg_color = Some(p.well);
    v.window_stroke = Stroke::new(th.shape.stroke, p.border);
    v.window_corner_radius = cr(th.shape.radius_large);
    v.menu_corner_radius = cr(th.shape.radius);
    v.window_shadow.color = p.shadow;
    v.popup_shadow.color = p.shadow;
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(th.shape.focus_width, p.focus);
    v.hyperlink_color = p.accent_ink;
    v.text_cursor.stroke = Stroke::new(2.0, p.text);
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    let ws = &mut v.widgets;
    for (wv, fill) in [
        (&mut ws.noninteractive, p.surface),
        (&mut ws.inactive, p.control),
        (&mut ws.hovered, p.control_hover),
        (&mut ws.active, p.control_hover),
        (&mut ws.open, p.control),
    ] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.fg_stroke = Stroke::new(1.0, p.text);
        wv.bg_stroke = Stroke::NONE;
        wv.corner_radius = cr(th.shape.radius);
    }
    ws.noninteractive.bg_stroke = Stroke::new(th.shape.stroke, p.border);
    let t = &th.type_;
    let sp = th.shape.spacing;
    for which in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.set_visuals_of(which, v.clone());
        ctx.style_mut_of(which, |s| {
            s.text_styles = [
                (
                    TextStyle::Small,
                    FontId::new(t.size_small, FontFamily::Proportional),
                ),
                (
                    TextStyle::Body,
                    FontId::new(t.size_body, FontFamily::Proportional),
                ),
                (
                    TextStyle::Button,
                    FontId::new(t.size_body, crate::fonts::medium()),
                ),
                (
                    TextStyle::Heading,
                    FontId::new(t.size_heading, crate::fonts::medium()),
                ),
                (
                    TextStyle::Monospace,
                    FontId::new(t.size_body, FontFamily::Monospace),
                ),
            ]
            .into();
            s.spacing.item_spacing = vec2(sp, sp);
            s.spacing.button_padding = vec2(sp * 1.5, sp);
        });
    }
}

/// The status line at the bottom of the sidebar: one dot, one sentence.
fn status_line(tk: &Tk, conn: &Conn) -> (egui::Color32, String) {
    let p = tk.p;
    match conn {
        Conn::Loading => (p.text_muted, tk.t("side-loading")),
        Conn::Absent { .. } => (p.warning, tk.t("side-not-running")),
        Conn::Live { snap, .. } => {
            let s = &snap.state;
            if let Some((h, session)) = s.connected() {
                if s.displays.iter().any(|d| d.state == DisplayState::Failed) {
                    (p.danger, tk.t("side-failed"))
                } else if !session.worn {
                    (
                        p.text_muted,
                        tk.f("side-taken-off", &[("name", (&h.name).into())]),
                    )
                } else {
                    (p.ok, tk.f("side-connected", &[("name", (&h.name).into())]))
                }
            } else if let Some(name) = &s.connecting {
                (
                    p.accent_ink,
                    tk.f("side-connecting", &[("name", name.into())]),
                )
            } else if s.headsets.is_empty() {
                (p.text_muted, tk.t("side-no-headsets"))
            } else {
                (p.text_muted, tk.t("side-waiting"))
            }
        }
    }
}

fn sidebar(
    ui: &mut Ui,
    tk: &Tk,
    conn: &Conn,
    st: &UiState,
    first_run: bool,
    out: &mut Vec<Action>,
) {
    let p = tk.p;
    ui.horizontal(|ui| {
        w::brand_mark(ui, tk, 30.0);
        ui.add_space(2.0);
        text(
            ui,
            "Prostir",
            tk.font(tk.th.type_.size_heading + 3.0, true),
            p.text,
        );
        text(ui, env!("CARGO_PKG_VERSION"), tk.small(), p.text_muted);
    });
    ui.add_space(tk.th.shape.section_gap);
    for (page, id) in [
        (Page::Overview, "nav-overview"),
        (Page::Monitors, "nav-monitors"),
        (Page::Headsets, "nav-headsets"),
        (Page::Sound, "nav-sound"),
        (Page::Look, "nav-look"),
    ] {
        nav_item(ui, tk, st.page, page, id, out);
    }
    if first_run {
        ui.add_space(tk.th.shape.spacing);
        let (r, _) = ui.allocate_exact_size(
            vec2(ui.available_width(), tk.th.shape.stroke.max(1.0)),
            Sense::hover(),
        );
        ui.painter()
            .rect_filled(r.shrink2(vec2(12.0, 0.0)), 0.0, p.border);
        ui.add_space(tk.th.shape.spacing);
        nav_item(ui, tk, st.page, Page::FirstRun, "nav-first-run", out);
    }
    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
        let (c, s) = status_line(tk, conn);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(12.0, 20.0), Sense::hover());
            w::dot(ui, tk, r.center(), c);
            text(ui, s, tk.small(), p.text);
        });
    });
}

fn nav_item(ui: &mut Ui, tk: &Tk, current: Page, page: Page, id: &str, out: &mut Vec<Action>) {
    let p = tk.p;
    let (rect, resp) = ui.allocate_exact_size(
        vec2(ui.available_width(), tk.th.shape.control_height),
        Sense::click(),
    );
    let on = current == page;
    let h = ui.ctx().animate_bool_with_time(
        resp.id.with("h"),
        resp.hovered(),
        tk.secs(tk.th.motion.hover_ms),
    );
    let fill = if on {
        p.control
    } else {
        w::faded(p.control, h * 0.6)
    };
    ui.painter().rect_filled(rect, tk.r(), fill);
    let label = tk.t(id);
    let font = if on { tk.body_m() } else { tk.body() };
    let g =
        ui.painter()
            .layout_no_wrap(label.clone(), font, if on { p.text } else { p.text_muted });
    let tp = egui::pos2(rect.left() + 14.0, rect.center().y - g.size().y / 2.0);
    let tw = g.size().x;
    ui.painter().galley(tp, g, p.text);
    if on {
        // The headset menu marks its open tab with a blue line under the name.
        let bar = egui::Rect::from_min_size(egui::pos2(tp.x, rect.bottom() - 7.0), vec2(tw, 3.0));
        ui.painter()
            .rect_filled(bar, egui::CornerRadius::same(2), p.accent_ink);
    }
    tk.focus_ring(ui, &resp, tk.th.shape.radius);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, on, &label));
    if resp.clicked() {
        out.push(Action::Go(page));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand);
}

/// A message after an action, above the page, until closed.
fn notice(ui: &mut Ui, tk: &Tk, s: &str, out: &mut Vec<Action>) {
    egui::Frame::new()
        .fill(tk.p.surface)
        .stroke(Stroke::new(tk.th.shape.stroke, tk.p.danger))
        .corner_radius(tk.r())
        .inner_margin(egui::Margin::same((tk.th.shape.padding * 0.6).round() as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let close = tk.t("notice-close");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if w::button_named(ui, tk, &tk.t("pair-close"), &close, Kind::Quiet, true)
                        .clicked()
                    {
                        out.push(Action::Dismiss);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        w::status(ui, tk, tk.p.danger, s);
                    });
                });
            });
        });
    ui.add_space(tk.th.shape.section_gap * 0.6);
}
