//! OpenXR main loop.
//!
//! Each KDE monitor is decoded by MediaCodec straight into an Android surface
//! owned by the Quest compositor (XR_KHR_android_surface_swapchain) and shown as
//! a quad or cylinder layer. The menu, its button, pointer dots and the
//! panorama are small CPU-drawn images; nothing is rendered per frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use anyhow::{Result, anyhow, bail};
use glam::{Vec2, Vec3};
use log::{error, info, warn};
use openxr as xr;
use openxr::sys::Handle as _;
use prostir_proto::{ClientMsg, MIRROR_ID};

use crate::i18n::{Lang, Language};
use crate::input::Controls;
use crate::layout::{self, Cylinder, Grab, Hit, Placement, Ray};
use crate::net::{Feedback, NetEvent};
use crate::overlay::{self, Overlay, Sphere};
use crate::profiles::{self, Profile};
use crate::settings::{Background, HandMode, Settings};
use crate::ui::{self, Action, MonitorEntry, Panel, Tab, UiState};
use crate::xrutil::{self, Gfx, ImageLayer, as_layer, pose, posef, vec3};
use crate::{background, decoder, egl, net, passthrough};

/// Grab id for the menu panel (monitor ids are 0..=MIRROR_ID).
const PANEL_ID: u8 = 250;
/// Grab id for the keyboard window.
const KEYBOARD_ID: u8 = 251;
/// The keyboard window's depth as a share of its width (a keyboard's shape).
const KEYBOARD_ASPECT: f32 = 0.44;
const PANEL_WIDTH: f32 = 0.62;
const BUTTON_SIZE: f32 = 0.075;
const CURSOR_PX: u32 = 64;
const BUTTON_PX: u32 = 128;
const ERROR_SHOWN_FOR: Duration = Duration::from_secs(8);
const CONFIRM_FOR: Duration = Duration::from_secs(4);
/// How long a pinch must be held on a monitor before it starts dragging it.
const HOLD_TO_DRAG: Duration = Duration::from_millis(300);

/// `XR_HAND_JOINT_INDEX_TIP_EXT`.
const INDEX_TIP: usize = 10;
/// Touching the menu with a fingertip, like a touchscreen. Distances from the menu's
/// surface, positive in front of it.
const POKE_HOVER: f32 = 0.06;
const POKE_PRESS: f32 = 0.008;
const POKE_RELEASE: f32 = 0.025;
/// A finger further behind the menu than this is not touching it (reaching past).
const POKE_BEHIND: f32 = -0.06;

/// A monitor held with both hands: the distance between them sets its width.
#[derive(Clone, Copy)]
struct Scaling {
    id: u8,
    /// The hand that took hold second (the first one keeps its ordinary grab).
    hand: usize,
    start_distance: f32,
    start_width: f32,
}

/// One fingertip against the menu.
#[derive(Default, Clone, Copy)]
struct Poke {
    /// Distance to the menu in the previous frame, while in front of it.
    last_z: Option<f32>,
    /// Touched and not yet pulled back.
    pressed: bool,
    /// Dragging the menu: the touched point in the menu's own plane.
    drag: Option<Vec2>,
    /// The last touch: pulling the hand back often closes thumb and index into a
    /// pinch, which must not click whatever this hand's ray points at.
    touched_at: Option<Instant>,
}

/// Pinches of a hand that just touched the menu are ignored this long.
const AFTER_TOUCH: Duration = Duration::from_millis(600);

#[unsafe(no_mangle)]
pub fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(if cfg!(debug_assertions) {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            })
            .with_tag("prostir"),
    );
    if let Err(e) = run(&app) {
        error!("{e:#}");
    }
}

fn panel_height() -> f32 {
    PANEL_WIDTH * ui::HEIGHT as f32 / ui::WIDTH as f32
}

/// One stream: the compositor surface it is decoded into and its decoder thread.
struct Monitor {
    width_px: u32,
    height_px: u32,
    decoder: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    swapchain: xr::Swapchain<Gfx>,
}

impl Monitor {
    fn aspect(&self) -> f32 {
        self.height_px as f32 / self.width_px as f32
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // The decoder must stop writing into the surface before the swapchain goes away.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.decoder.take() {
            let _ = t.join();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    Panel,
    MenuButton,
    Monitor(u8),
    Keyboard,
}

/// Lying flat (facing up) with its far edge pointing away from `head`, like a
/// keyboard on a desk.
fn lying_flat(position: Vec3, head: Vec3) -> glam::Quat {
    let away = Vec3::new(position.x - head.x, 0.0, position.z - head.z).normalize_or(-Vec3::Z);
    glam::Quat::from_rotation_y((-away.x).atan2(-away.z))
        * glam::Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)
}

/// Everything shown in the headset and how it is arranged.
struct Scene {
    monitors: HashMap<u8, Monitor>,
    /// Left-to-right order of the Quest monitors, shared with the host and KDE.
    order: Vec<u8>,
    placements: HashMap<u8, Placement>,
    primary: Option<u8>,
    mirror_wanted: bool,
    audio_wanted: bool,
    mic_wanted: bool,
    /// The monitor the mouse is on (from the PC).
    focus: Option<u8>,
    /// Current brightness of each monitor, easing towards 1 or the dimmed level.
    brightness: HashMap<u8, f32>,
    /// Resizing a monitor with both hands.
    scaling: Option<Scaling>,
    /// Where each monitor is drawn: follows `placements`, easing into new spots.
    shown: HashMap<u8, Placement>,
    /// How far each monitor has faded in (0 to 1) since it appeared.
    appear: HashMap<u8, f32>,
    /// The headset is on the head (the session is visible); streaming pauses otherwise.
    worn: bool,
    settings: Settings,
    curve_supported: bool,
    panel_open: bool,
    panel_place: Placement,
    button_place: Option<Placement>,
    grabs: [Option<Grab>; 2],
    host: Option<String>,
    /// PCs announcing themselves while not connected.
    found: Vec<net::FoundHost>,
    /// IP typed on the menu keypad.
    ip_input: Option<String>,
    /// Pairing with a new PC in progress.
    pairing: Option<ui::PairingView>,
    /// A PC on the network that refuses the connection (a firewall).
    unreachable: Option<String>,
    /// An unknown PC waiting for "Pair" or "Not now" (survives the disconnect that
    /// follows the question).
    pair_offer: Option<(String, bool)>,
    steering: net::Steering,
    /// When the connection was lost (or the app started without one).
    offline_since: Option<Instant>,
    feedback: Option<Feedback>,
    error: Option<(String, Instant)>,
    /// "×" pressed once on this monitor; a second press within a few seconds removes it.
    confirm_remove: Option<(u8, Instant)>,
    /// Menu page shown.
    tab: Tab,
    /// The PC speaks another protocol: (the PC is newer, its app version).
    update_needed: Option<(bool, String)>,
    /// Where layouts are centred and which way they face (head position, yaw).
    center: (Vec3, f32),
    /// The arrangement last sent to the PC.
    sent_layout: Option<Vec<prostir_proto::LayoutRow>>,
    /// A changed arrangement waiting to settle before it is sent.
    pending_layout: Option<(Vec<prostir_proto::LayoutRow>, Instant)>,
    /// Head position and gaze this frame.
    head: (Vec3, Vec3),
    /// Tracked hands are in view this frame (the menu then opens within reach).
    hands_in_view: bool,
    /// Where the keyboard window lies (set the first time it is shown).
    keyboard_place: Option<Placement>,
    keyboard_path: PathBuf,
    /// The headset's own system language.
    system_lang: Option<Lang>,
    profiles: [Profile; profiles::SLOTS],
    profiles_path: PathBuf,
    /// The profile last switched to, until something is changed by hand.
    active_profile: Option<usize>,
    /// After switching profiles, monitors come and go for a few seconds: lay them
    /// out again on each change until then.
    relayout_until: Option<Instant>,
    layout_path: PathBuf,
    settings_path: PathBuf,
}

impl Scene {
    /// Quest monitors left to right: in the host's order, unknown ones at the end.
    fn quest_sizes(&self) -> Vec<(u8, u32, u32)> {
        let mut ids: Vec<u8> = self
            .order
            .iter()
            .copied()
            .filter(|id| self.monitors.contains_key(id))
            .collect();
        let mut rest: Vec<u8> = self
            .monitors
            .keys()
            .copied()
            .filter(|id| *id != MIRROR_ID && !ids.contains(id))
            .collect();
        rest.sort_unstable();
        ids.extend(rest);
        ids.into_iter()
            .map(|id| {
                let m = &self.monitors[&id];
                (id, m.width_px, m.height_px)
            })
            .collect()
    }

    /// Sends a new left-to-right order to the host (which re-arranges KDE to match).
    /// Tells the PC how the monitors stand around the user (rows, left to right), so
    /// KDE's arrangement matches: the mouse leaves a monitor where the next one is,
    /// sideways, up or down. Only sent when it changed.
    fn send_layout(&mut self) {
        if self.settings.cinema || self.feedback.is_none() {
            return;
        }
        // Wait until every monitor the PC has is here and placed: a partial picture
        // would make KDE rearrange (and blink) once per arriving monitor.
        let complete = !self.order.is_empty()
            && self
                .order
                .iter()
                .all(|id| self.monitors.contains_key(id) && self.placements.contains_key(id));
        if !complete {
            return;
        }
        let rows = layout::rows_by_position(&self.placements, &self.quest_sizes(), self.center);
        if rows.is_empty() || self.sent_layout.as_ref() == Some(&rows) {
            self.pending_layout = None;
            return;
        }
        // Only an arrangement that has held for a moment: monitors arriving one by one
        // or being laid out again pass through states KDE should not be shown.
        match &self.pending_layout {
            Some((pending, since)) if *pending == rows => {
                if since.elapsed() < Duration::from_millis(800) {
                    return;
                }
            }
            _ => {
                self.pending_layout = Some((rows, Instant::now()));
                return;
            }
        }
        self.pending_layout = None;
        info!(
            "KDE arrangement: {}",
            rows.iter()
                .map(|r| format!("{:?}@{}", r.ids, r.offset))
                .collect::<Vec<_>>()
                .join(" / ")
        );
        self.order = rows.iter().flat_map(|r| r.ids.clone()).collect();
        self.sent_layout = Some(rows.clone());
        self.send(ClientMsg::SetLayout { rows });
    }

    fn mirror_size(&self) -> Option<(u8, u32, u32)> {
        self.monitors
            .get(&MIRROR_ID)
            .map(|m| (MIRROR_ID, m.width_px, m.height_px))
    }

    /// Puts the laptop mirror back under the primary monitor, leaving the rest alone.
    fn place_mirror(&mut self) {
        let all = self.around_user(layout::arrange_curved(
            self.settings.preset,
            &self.quest_sizes(),
            self.primary,
            self.mirror_size(),
            self.curve_radius(),
        ));
        if let Some(p) = all.get(&MIRROR_ID) {
            self.placements.insert(MIRROR_ID, *p);
        }
    }

    /// Moves the drawn monitors towards their places (held ones follow at once) and
    /// fades new ones in.
    fn animate(&mut self, dt: f32, held: &[u8]) {
        // About a quarter of a second to settle.
        let k = 1.0 - (-dt * 12.0).exp();
        for (id, target) in &self.placements {
            match self.shown.get_mut(id) {
                Some(s) if !held.contains(id) => {
                    s.position = s.position.lerp(target.position, k);
                    s.rotation = s.rotation.slerp(target.rotation, k);
                    s.width += (target.width - s.width) * k;
                    if s.position.distance(target.position) < 0.0005 {
                        *s = *target;
                    }
                }
                Some(s) => *s = *target,
                None => {
                    self.shown.insert(*id, *target);
                }
            }
        }
        let placements = &self.placements;
        self.shown.retain(|id, _| placements.contains_key(id));
        for id in self.monitors.keys() {
            let a = self.appear.entry(*id).or_insert(0.0);
            *a = (*a + dt / 0.3).min(1.0);
        }
        let monitors = &self.monitors;
        self.appear.retain(|id, _| monitors.contains_key(id));
    }

    /// The setup as it is now: monitors left to right, the primary, the laptop screen,
    /// layout and look.
    fn current_profile(&self) -> Profile {
        let sizes = self.quest_sizes();
        Profile {
            primary: sizes
                .iter()
                .position(|m| Some(m.0) == self.primary)
                .unwrap_or(0) as u8,
            sizes: sizes.iter().map(|m| (m.1, m.2)).collect(),
            mirror: self.mirror_wanted,
            settings: self.settings,
        }
    }

    fn relayout_pending(&self) -> bool {
        self.relayout_until.is_some_and(|t| Instant::now() < t)
    }

    /// Lays everything out by the chosen preset.
    /// Layouts are worked out around the origin facing -Z; this moves them to where
    /// the user sat and looked when they last asked to bring everything in front.
    fn around_user(&self, mut placements: HashMap<u8, Placement>) -> HashMap<u8, Placement> {
        let (center, yaw) = self.center;
        let turn = glam::Quat::from_rotation_y(yaw);
        for p in placements.values_mut() {
            p.position = center + turn * p.position;
            p.rotation = turn * p.rotation;
        }
        placements
    }

    fn arrange_all(&mut self) {
        if self.settings.cinema {
            self.placements = self.around_user(layout::cinema(
                &self.quest_sizes(),
                self.settings.cinema_screen,
            ));
            self.grabs = [None, None];
            return;
        }
        let mirror_place = self.placements.get(&MIRROR_ID).copied();
        self.placements = self.around_user(layout::arrange_curved(
            self.settings.preset,
            &self.quest_sizes(),
            self.primary,
            self.mirror_size(),
            self.curve_radius(),
        ));
        // Keep the mirror's spot while the laptop lid is closed.
        if let (Some(p), false) = (mirror_place, self.placements.contains_key(&MIRROR_ID)) {
            self.placements.insert(MIRROR_ID, p);
        }
        self.grabs = [None, None];
        layout::save(&self.layout_path, &self.placements);
        self.send_layout();
    }

    fn curve_radius(&self) -> Option<f32> {
        if !self.curve_supported {
            return None;
        }
        self.settings
            .curve
            .radius_factor()
            .map(|f| f * layout::RADIUS)
    }

    fn height_of(&self, id: u8) -> f32 {
        match (self.placements.get(&id), self.monitors.get(&id)) {
            (Some(p), Some(m)) => p.width * m.aspect(),
            _ => 0.0,
        }
    }

    /// Nearest thing the ray points at.
    fn pick(&self, ray: Ray) -> Option<(Target, Hit)> {
        let mut best: Option<(Target, Hit)> = None;
        let mut consider = |target: Target, hit: Option<Hit>| {
            if let Some(h) = hit
                && best.is_none_or(|(_, b)| h.t < b.t)
            {
                best = Some((target, h));
            }
        };
        if self.panel_open {
            consider(
                Target::Panel,
                layout::hit_surface(ray, &self.panel_place, panel_height(), None),
            );
        }
        if self.settings.keyboard_window
            && let Some(k) = &self.keyboard_place
        {
            consider(
                Target::Keyboard,
                layout::hit_surface(ray, k, k.width * KEYBOARD_ASPECT, None),
            );
        }
        if let Some(b) = &self.button_place {
            consider(
                Target::MenuButton,
                layout::hit_surface(ray, b, BUTTON_SIZE, None),
            );
        }
        let curve = self.curve_radius();
        for id in self.monitors.keys() {
            if let Some(p) = self.placements.get(id) {
                consider(
                    Target::Monitor(*id),
                    layout::hit_surface(ray, p, self.height_of(*id), curve),
                );
            }
        }
        best
    }

    /// Puts the menu button just under the primary monitor's bottom-left corner, and
    /// left of the laptop mirror hanging there so it never covers it. When the primary
    /// is not shown (cinema on another screen), under the first monitor that is: with
    /// hands, the button is the only way back to the menu.
    fn place_button(&mut self) {
        let shown = |id: &u8| self.placements.contains_key(id);
        let anchor = self
            .primary
            .filter(shown)
            .or_else(|| self.quest_sizes().iter().map(|m| m.0).find(|id| shown(id)));
        let mirror = self
            .monitors
            .contains_key(&MIRROR_ID)
            .then(|| self.placements.get(&MIRROR_ID).copied())
            .flatten()
            .map(|m| (m, self.height_of(MIRROR_ID)));
        self.button_place = anchor.and_then(|id| {
            let p = self.placements.get(&id)?;
            let h = self.height_of(id);
            let mut x = -p.width / 2.0 + BUTTON_SIZE;
            if let Some((m, mh)) = mirror {
                let corner = m.position + m.rotation * Vec3::new(-m.width / 2.0, mh / 2.0, 0.0);
                let local = p.rotation.inverse() * (corner - p.position);
                // Only when the mirror actually hangs under this monitor.
                if local.y < -h / 2.0 + 0.05 && local.y > -h / 2.0 - 0.3 {
                    x = x.min(local.x - BUTTON_SIZE * 0.8);
                }
            }
            let offset = Vec3::new(x, -h / 2.0 - BUTTON_SIZE * 0.9, 0.02);
            Some(Placement {
                position: p.position + p.rotation * offset,
                rotation: p.rotation,
                width: BUTTON_SIZE,
            })
        });
    }

    /// Opens the menu in front of the user: within arm's reach, a little lower, when
    /// hands are in view (to touch it), further away for controllers.
    fn open_panel(&mut self, head: Vec3, forward: Vec3, touch: bool) {
        let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(-Vec3::Z);
        let position = if touch {
            head + flat * 0.5 - Vec3::Y * 0.22
        } else {
            head + flat * 0.7 - Vec3::Y * 0.12
        };
        self.panel_place = Placement {
            position,
            rotation: layout::facing(position, head),
            width: PANEL_WIDTH,
        };
        self.panel_open = true;
    }

    /// Tells the PC whether the headset is worn (quietly: nothing to show the user).
    fn tell_presence(&self) {
        if let Some(f) = &self.feedback {
            let _ = f.send(&ClientMsg::Presence { worn: self.worn });
        }
    }

    fn send(&mut self, msg: ClientMsg) {
        let sent = self.feedback.as_ref().map(|f| f.send(&msg));
        match sent {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                let what = self
                    .lang()
                    .t("Не вдалося надіслати на ПК", "Could not send to the PC");
                self.show_error(format!("{what}: {e}"))
            }
            None => {
                let what = self
                    .lang()
                    .t("Немає з'єднання з ПК", "Not connected to a PC");
                self.show_error(what.into())
            }
        }
    }

    /// The interface language: chosen in the menu, else the PC's, else the headset's.
    fn lang(&self) -> Lang {
        match self.settings.language {
            Language::Fixed(l) => l,
            Language::LikePc => self
                .settings
                .pc_language
                .or(self.system_lang)
                .unwrap_or(Lang::En),
        }
    }

    fn show_error(&mut self, text: String) {
        warn!("{text}");
        self.error = Some((text, Instant::now()));
    }

    fn ui_state(&self, room: bool, keyboard: bool, panorama: bool, custom: bool) -> UiState {
        UiState {
            host: self.host.clone(),
            found: self
                .found
                .iter()
                .map(|h| (h.name.clone(), h.addr.ip().to_string()))
                .collect(),
            ip_input: self.ip_input.clone(),
            pairing: self.pairing.clone(),
            pair_offer: self.pair_offer.clone(),
            unreachable: self.unreachable.clone(),
            monitors: self
                .quest_sizes()
                .into_iter()
                .map(|(id, width, height)| MonitorEntry { id, width, height })
                .collect(),
            primary: self.primary,
            mirror_wanted: self.mirror_wanted,
            audio_wanted: self.audio_wanted,
            mic_wanted: self.mic_wanted,
            mic_denied: self.mic_wanted && crate::mic::denied(),
            mirror_active: self.monitors.contains_key(&MIRROR_ID),
            settings: self.settings,
            room_available: room,
            panorama_available: panorama,
            custom_available: custom,
            curve_available: self.curve_supported,
            keyboard_available: keyboard,
            error: self
                .error
                .as_ref()
                .filter(|(_, at)| at.elapsed() < ERROR_SHOWN_FOR)
                .map(|(t, _)| t.clone()),
            confirm_remove: self
                .confirm_remove
                .filter(|(_, at)| at.elapsed() < CONFIRM_FOR)
                .map(|(id, _)| id),
            tab: self.tab,
            lang: self.lang(),
            intro: !self.settings.intro_seen,
            update: self.update_needed.clone(),
            profiles: self.profiles.to_vec(),
            active_profile: self.active_profile,
        }
    }
}

/// The user's own panorama (`background.jpg`/`.png` next to `host.txt`), else the night sky.
/// The user's own panorama (`background.jpg/.png` next to `host.txt`), if there is one.
fn custom_panorama_path(dir: Option<&Path>) -> Option<PathBuf> {
    let d = dir?;
    ["background.jpg", "background.jpeg", "background.png"]
        .iter()
        .map(|n| d.join(n))
        .find(|p| p.exists())
}

/// Width, height and RGBA pixels of a panorama.
type Panorama = (u32, u32, Vec<u8>);

/// Pixels for a panorama background: generated sky or the user's picture.
fn load_panorama(choice: Background, dir: Option<PathBuf>) -> Panorama {
    if choice == Background::Custom
        && let Some(path) = custom_panorama_path(dir.as_deref())
    {
        match image::open(&path) {
            Ok(img) => {
                let img = if img.width() > 4096 {
                    img.resize(4096, 2048, image::imageops::FilterType::Triangle)
                } else {
                    img
                };
                let rgba = img.to_rgba8();
                info!("panorama from {}", path.display());
                return (rgba.width(), rgba.height(), rgba.into_raw());
            }
            Err(e) => warn!("cannot read {}: {e}", path.display()),
        }
    }
    let sky = match choice {
        Background::Sky(sky) => sky,
        _ => background::Sky::Night,
    };
    (
        background::WIDTH,
        background::HEIGHT,
        background::generate(sky),
    )
}

fn run(app: &AndroidApp) -> Result<()> {
    // SAFETY: pointers come straight from the running NativeActivity.
    let platform = unsafe { xr::AndroidPlatformInfo::new(app.vm_as_ptr(), app.activity_as_ptr()) };
    // SAFETY: libopenxr_loader.so is the Khronos loader bundled in the APK.
    let entry = unsafe { xr::Entry::load(&platform) }
        .map_err(|e| anyhow!("cannot load OpenXR loader: {e}"))?;
    let available = entry.enumerate_extensions()?;
    if !available.khr_android_surface_swapchain {
        bail!("the runtime lacks XR_KHR_android_surface_swapchain");
    }
    let mut exts = xr::ExtensionSet::default();
    exts.khr_android_create_instance = true;
    exts.khr_opengl_es_enable = true;
    exts.khr_android_surface_swapchain = true;
    exts.fb_passthrough = available.fb_passthrough;
    exts.fb_display_refresh_rate = available.fb_display_refresh_rate;
    exts.fb_composition_layer_settings = available.fb_composition_layer_settings;
    exts.fb_composition_layer_image_layout = available.fb_composition_layer_image_layout;
    exts.khr_composition_layer_cylinder = available.khr_composition_layer_cylinder;
    exts.khr_composition_layer_equirect2 = available.khr_composition_layer_equirect2;
    exts.ext_hand_interaction = available.ext_hand_interaction;
    exts.ext_hand_tracking = available.ext_hand_tracking;
    exts.fb_hand_tracking_aim = available.fb_hand_tracking_aim;
    exts.fb_triangle_mesh = available.fb_triangle_mesh;
    exts.khr_composition_layer_color_scale_bias = available.khr_composition_layer_color_scale_bias;
    let instance = entry.create_instance(
        &xr::ApplicationInfo {
            application_name: "Prostir",
            application_version: 1,
            engine_name: "prostir",
            engine_version: 1,
            api_version: xr::Version::new(1, 0, 0),
        },
        &exts,
        &[],
        &platform,
    )?;
    info!(
        "Prostir {}: cylinder layers: {}, equirect: {}, hand interaction: {}",
        env!("CARGO_PKG_VERSION"),
        exts.khr_composition_layer_cylinder,
        exts.khr_composition_layer_equirect2,
        exts.ext_hand_interaction
    );
    let system = instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)?;
    let _ = instance.graphics_requirements::<Gfx>(system)?;

    let egl = egl::EglContext::new()?;
    // SAFETY: the EGL handles stay alive (owned by `egl`) for the whole session.
    let (session, mut frame_waiter, mut frame_stream) = unsafe {
        instance.create_session::<Gfx>(
            system,
            &xr::opengles::SessionCreateInfo::Android {
                display: egl.display.as_ptr().cast(),
                config: egl.config.as_ptr().cast(),
                context: egl.context.as_ptr().cast(),
            },
        )
    }?;
    let gl = xrutil::load_gl(&egl.egl);

    let data_dir: PathBuf = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    let settings_path = data_dir.join("settings.txt");
    let profiles_path = data_dir.join("profiles.txt");
    let settings = Settings::load(&settings_path);
    crate::mic::set_allowed(settings.mic);

    // Passthrough keeps the cameras busy and costs battery: only when "Room" is chosen.
    let mut passthrough = if exts.fb_passthrough {
        passthrough::Passthrough::new(&session, settings.background == Background::Room)
            .inspect_err(|e| warn!("passthrough unavailable: {e:#}"))
            .ok()
    } else {
        None
    };

    let local =
        session.create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)?;
    let view = session.create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)?;
    let mut controls = Controls::new(&instance, &session, exts.ext_hand_interaction)?;
    controls.hands_enabled = settings.hands == HandMode::Control;

    let format = xrutil::color_format(&session);
    let mut panel_layer = ImageLayer::new(&session, format, ui::WIDTH, ui::HEIGHT, false)?;
    let mut button_layer = ImageLayer::new(&session, format, BUTTON_PX, BUTTON_PX, false)?;
    let mut cursor_layer = ImageLayer::new(&session, format, CURSOR_PX, CURSOR_PX, true)?;
    cursor_layer.upload(&gl, &ui::cursor_image(CURSOR_PX))?;
    let mut button_hovered = false;
    // The left hand's menu gesture as last seen, to act once per gesture.
    let mut menu_gesture_held = false;
    button_layer.upload(&gl, &ui::menu_button_image(BUTTON_PX, false))?;
    let mut panel = Panel::new();
    // Tracked hands and pointer rays, drawn on top of everything.
    let hand_trackers = if exts.ext_hand_tracking {
        let make = |h| session.create_hand_tracker(h);
        make(xr::Hand::LEFT)
            .and_then(|l| Ok([l, make(xr::Hand::RIGHT)?]))
            .inspect_err(|e| warn!("hand tracking unavailable: {e}"))
            .ok()
    } else {
        None
    };
    let mut overlay = Overlay::new(&gl, &instance, system, &session, format)
        .inspect_err(|e| warn!("hand/ray overlay unavailable: {e:#}"))
        .ok();
    // The panorama shown and the background it was made for.
    let mut pano_layer: Option<(Background, ImageLayer)> = None;
    let mut pano_job: Option<(Background, JoinHandle<Panorama>)> = None;
    let external_dir = app.external_data_path();
    let mut custom_pano = custom_panorama_path(external_dir.as_deref()).is_some();
    let mut panel_was_open = false;

    let (net_tx, net_rx) = mpsc::channel();
    let steering = net::Steering::default();
    net::spawn(
        net_tx,
        net::Discovery {
            identity_file: data_dir.join("identity"),
            trust_file: data_dir.join("paired_hosts"),
            host_file: external_dir.as_ref().map(|p| p.join("host.txt")),
            last_host_file: data_dir.join("last_host"),
        },
        steering.clone(),
    );

    // SAFETY: the VM pointer is valid for the life of the process.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }?;
    let _wifi_lock = xrutil::acquire_low_latency_wifi(app, &vm)
        .inspect(|_| info!("Wi-Fi low-latency mode on"))
        .inspect_err(|e| warn!("Wi-Fi low-latency lock unavailable: {e:#}"))
        .ok();

    let layout_path = data_dir.join("layout.txt");
    let mut scene = Scene {
        monitors: HashMap::new(),
        order: Vec::new(),
        placements: layout::load(&layout_path),
        primary: None,
        mirror_wanted: true,
        audio_wanted: true,
        mic_wanted: settings.mic,
        focus: None,
        brightness: HashMap::new(),
        scaling: None,
        shown: HashMap::new(),
        appear: HashMap::new(),
        worn: true,
        settings,
        curve_supported: exts.khr_composition_layer_cylinder,
        panel_open: false,
        panel_place: Placement {
            position: Vec3::new(0.0, -0.12, -0.7),
            rotation: glam::Quat::IDENTITY,
            width: PANEL_WIDTH,
        },
        button_place: None,
        grabs: [None, None],
        host: None,
        found: Vec::new(),
        ip_input: None,
        pairing: None,
        pair_offer: None,
        unreachable: None,
        steering,
        offline_since: Some(Instant::now()),
        feedback: None,
        error: None,
        confirm_remove: None,
        tab: Tab::Monitors,
        update_needed: None,
        center: (Vec3::ZERO, 0.0),
        sent_layout: None,
        pending_layout: None,
        head: (Vec3::ZERO, -Vec3::Z),
        hands_in_view: false,
        keyboard_place: layout::load(&data_dir.join("keyboard.txt")).remove(&0),
        keyboard_path: data_dir.join("keyboard.txt"),
        system_lang: crate::i18n::system(),
        profiles: profiles::load(&profiles_path),
        profiles_path,
        active_profile: None,
        relayout_until: None,
        layout_path,
        settings_path,
    };

    let mut hold_since: [Option<Instant>; 2] = [None, None];
    let mut pokes = [Poke::default(); 2];
    // Store builds: the store requires checking the user owns the app at launch.
    #[cfg(feature = "store")]
    let mut entitlement = crate::store::Entitlement::start();
    #[cfg(feature = "store")]
    let mut not_entitled_since: Option<Instant> = entitlement.is_none().then(Instant::now);
    let mut session_running = false;
    let mut last_time: Option<xr::Time> = None;
    let mut event_storage = xr::EventDataBuffer::new();

    'main: loop {
        let mut destroy = false;
        let timeout = if session_running {
            Duration::ZERO
        } else {
            Duration::from_millis(100)
        };
        let mut input_waiting = false;
        app.poll_events(Some(timeout), |event| match event {
            PollEvent::Main(MainEvent::Destroy) => destroy = true,
            PollEvent::Main(MainEvent::InputAvailable) => input_waiting = true,
            _ => {}
        });
        if destroy {
            break;
        }
        #[cfg(feature = "store")]
        {
            if entitlement.as_mut().and_then(|e| e.poll()) == Some(false)
                && not_entitled_since.is_none()
            {
                not_entitled_since = Some(Instant::now());
            }
            if let Some(since) = not_entitled_since {
                let what = scene.lang().t(
                    "Не вдалося підтвердити, що Prostir отримано з Meta Horizon Store",
                    "Could not confirm Prostir came from the Meta Horizon Store",
                );
                if scene.error.as_ref().is_none_or(|(t, _)| t != what) {
                    scene.show_error(what.into());
                    // In front of the user, wherever the menu was last.
                    let (head, forward) = scene.head;
                    let hands = scene.hands_in_view;
                    scene.open_panel(head, forward, hands);
                    // The 6 s count from when the message is up, not from the failure.
                    not_entitled_since = Some(Instant::now());
                }
                // Long enough to read, then close as the store expects.
                if since.elapsed() > Duration::from_secs(6) {
                    break;
                }
            }
        }
        if input_waiting {
            // Android waits for every key event (e.g. the volume buttons) to be taken
            // and answers "not responding" after 5 s otherwise. Hand them all back to
            // the system so the volume buttons keep working.
            if let Ok(mut events) = app.input_events_iter() {
                while events.next(|_| InputStatus::Unhandled) {}
            }
        }

        while let Some(event) = instance.poll_event(&mut event_storage)? {
            match event {
                xr::Event::SessionStateChanged(e) => {
                    info!("session state: {:?}", e.state());
                    // Taking the headset off hides the app (and puts the headset to
                    // sleep); the PC stops encoding until it is back on.
                    let worn = matches!(
                        e.state(),
                        xr::SessionState::VISIBLE | xr::SessionState::FOCUSED
                    );
                    if worn != scene.worn
                        && matches!(
                            e.state(),
                            xr::SessionState::VISIBLE
                                | xr::SessionState::FOCUSED
                                | xr::SessionState::SYNCHRONIZED
                                | xr::SessionState::STOPPING
                                | xr::SessionState::IDLE
                        )
                    {
                        scene.worn = worn;
                        scene.tell_presence();
                        scene.steering.set_worn(worn);
                    }
                    match e.state() {
                        xr::SessionState::READY => {
                            session.begin(xr::ViewConfigurationType::PRIMARY_STEREO)?;
                            session_running = true;
                            if exts.fb_display_refresh_rate {
                                let best =
                                    session
                                        .enumerate_display_refresh_rates()
                                        .ok()
                                        .and_then(|r| {
                                            r.into_iter().filter(|&r| r <= 90.0).reduce(f32::max)
                                        });
                                if let Some(rate) = best {
                                    let _ = session.request_display_refresh_rate(rate);
                                }
                            }
                        }
                        xr::SessionState::STOPPING => {
                            session.end()?;
                            session_running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => break 'main,
                        _ => {}
                    }
                }
                xr::Event::InstanceLossPending(_) => break 'main,
                _ => {}
            }
        }

        while let Ok(event) = net_rx.try_recv() {
            handle_net_event(&mut scene, &session, &vm, event);
        }

        // Panorama: generated or decoded off the main thread, uploaded once per choice.
        let wanted = scene.settings.background;
        let wants_pano = matches!(wanted, Background::Sky(_) | Background::Custom);
        if wants_pano
            && exts.khr_composition_layer_equirect2
            && pano_layer.as_ref().is_none_or(|(b, _)| *b != wanted)
            && pano_job.is_none()
        {
            let dir = external_dir.clone();
            pano_job = Some((
                wanted,
                std::thread::spawn(move || load_panorama(wanted, dir)),
            ));
        }
        if pano_job.as_ref().is_some_and(|(_, j)| j.is_finished()) {
            let (made_for, job) = pano_job.take().unwrap();
            match job.join() {
                Ok((w, h, pixels)) => {
                    let mut layer = ImageLayer::new(&session, format, w, h, true)?;
                    layer.upload(&gl, &pixels)?;
                    pano_layer = Some((made_for, layer));
                }
                Err(_) => warn!("panorama generation panicked"),
            }
        }

        if !session_running {
            continue;
        }

        let state = frame_waiter.wait()?;
        frame_stream.begin()?;
        let time = state.predicted_display_time;
        if !state.should_render {
            frame_stream.end(time, xr::EnvironmentBlendMode::OPAQUE, &[])?;
            continue;
        }
        let dt = last_time
            .map_or(0.0f32, |t| (time.as_nanos() - t.as_nanos()) as f32 * 1e-9)
            .clamp(0.0, 0.1);
        last_time = Some(time);

        controls.sync(&session, &local, time)?;
        let (head, forward) = view
            .locate(&local, time)
            .map_or((Vec3::ZERO, -Vec3::Z), |l| {
                (
                    vec3(l.pose.position),
                    xrutil::quat(l.pose.orientation) * -Vec3::Z,
                )
            });

        scene.head = (head, forward);

        // Tracked hands: joints for drawing, fingertips for touching the menu.
        let joints: [Option<[(Vec3, f32); 26]>; 2] = match &hand_trackers {
            Some(trackers) if scene.settings.hands != HandMode::Hidden => {
                std::array::from_fn(|h| {
                    local
                        .locate_hand_joints(&trackers[h], time)
                        .ok()
                        .flatten()
                        .and_then(|l| overlay::joints(&l))
                })
            }
            _ => [None, None],
        };
        let tips: [Option<Vec3>; 2] = std::array::from_fn(|h| joints[h].map(|j| j[INDEX_TIP].0));
        let hands_in_view = tips.iter().any(Option::is_some);
        scene.hands_in_view = hands_in_view;
        controls.hands_from_joints(&joints, head, forward);

        // Pairing needs the menu, and so do the first-run tips: open it where the
        // user is looking.
        let first_run =
            (!scene.settings.intro_seen && scene.host.is_some()) || scene.update_needed.is_some();
        if (scene.pairing.is_some() || scene.pair_offer.is_some() || first_run) && !scene.panel_open
        {
            scene.open_panel(head, forward, hands_in_view);
        }
        // Without a PC there is nothing to see: show the connection screen.
        if !scene.panel_open
            && scene
                .offline_since
                .is_some_and(|t| t.elapsed() > Duration::from_secs(4))
        {
            scene.open_panel(head, forward, hands_in_view);
            scene.offline_since = None;
        }
        // The left hand's menu gesture counts as the menu button.
        let gesture = match &hand_trackers {
            Some(t) if exts.fb_hand_tracking_aim => {
                xrutil::hand_menu_gesture(&instance, &t[0], &local, time)
            }
            _ => false,
        };
        let gesture_pressed = gesture && !menu_gesture_held;
        menu_gesture_held = gesture;
        if controls.menu_pressed(&session) || gesture_pressed {
            if scene.panel_open {
                scene.panel_open = false;
            } else {
                scene.open_panel(head, forward, hands_in_view);
            }
        }
        if controls.align_pressed(&session) {
            scene.arrange_all();
        }
        scene.place_button();

        // Pointing, clicking and dragging.
        let mut picks: [Option<(Target, Hit)>; 2] =
            std::array::from_fn(|h| controls.state[h].ray.and_then(|r| scene.pick(r)));
        let panel_point = |hit: &Hit| {
            let x = (hit.local.x / PANEL_WIDTH + 0.5) * ui::WIDTH as f32;
            let y = (0.5 - hit.local.y / panel_height()) * ui::HEIGHT as f32;
            (x, y)
        };

        // Touching the menu with a fingertip: hover when close, press on contact,
        // drag the menu by touching an empty spot.
        let mut poke_hover: [Option<(Vec2, f32)>; 2] = [None, None];
        for h in 0..2 {
            let poke = &mut pokes[h];
            let Some(tip) = tips[h].filter(|_| scene.panel_open) else {
                *poke = Poke {
                    touched_at: poke.touched_at,
                    ..Poke::default()
                };
                continue;
            };
            let place = scene.panel_place;
            let l = place.rotation.inverse() * (tip - place.position);
            let (half_w, half_h) = (PANEL_WIDTH / 2.0, panel_height() / 2.0);
            if let Some(anchor) = poke.drag {
                if l.z > POKE_RELEASE {
                    *poke = Poke {
                        touched_at: poke.touched_at,
                        ..Poke::default()
                    };
                    continue;
                }
                // The touched point follows the finger; pushing deeper moves it away.
                let shift = Vec3::new(l.x - anchor.x, l.y - anchor.y, (l.z + 0.03).min(0.0));
                let position = place.position + place.rotation * shift;
                scene.panel_place.position = position;
                scene.panel_place.rotation = layout::facing(position, head);
                picks[h] = None;
                continue;
            }
            let inside = l.x.abs() <= half_w && l.y.abs() <= half_h;
            if !inside || l.z > POKE_HOVER || l.z < POKE_BEHIND {
                poke.last_z = None;
                poke.pressed = false;
                continue;
            }
            // Near the menu, the finger is the pointer, not this hand's ray.
            picks[h] = None;
            let at = Vec2::new(l.x, l.y);
            poke_hover[h] = Some((at, l.z));
            let px = (
                (l.x / PANEL_WIDTH + 0.5) * ui::WIDTH as f32,
                (0.5 - l.y / panel_height()) * ui::HEIGHT as f32,
            );
            if poke.pressed && l.z > POKE_RELEASE {
                poke.pressed = false;
            }
            let arrived = poke.last_z.is_some_and(|z| z >= POKE_PRESS);
            // Hand tracking jitters: a finger "bouncing" off the menu is not a second tap.
            let settled = poke
                .touched_at
                .is_none_or(|t| t.elapsed() > Duration::from_millis(400));
            if !poke.pressed && arrived && settled && l.z < POKE_PRESS {
                poke.pressed = true;
                poke.touched_at = Some(Instant::now());
                panel.hover(Some(px));
                match panel.hovered_action() {
                    Some(action) => {
                        handle_action(&mut scene, action, &mut passthrough, &mut controls)
                    }
                    None => poke.drag = Some(at),
                }
            }
            poke.last_z = Some(l.z);
        }

        let poke_px = poke_hover.iter().flatten().next().map(|(at, _)| {
            (
                (at.x / PANEL_WIDTH + 0.5) * ui::WIDTH as f32,
                (0.5 - at.y / panel_height()) * ui::HEIGHT as f32,
            )
        });
        let hover = poke_px.or_else(|| {
            [1, 0].into_iter().find_map(|h| match picks[h] {
                Some((Target::Panel, hit)) => Some(panel_point(&hit)),
                _ => None,
            })
        });
        panel.hover(hover);
        let over_button = picks
            .iter()
            .any(|p| matches!(p, Some((Target::MenuButton, _))));
        if over_button != button_hovered {
            button_hovered = over_button;
            button_layer.upload(&gl, &ui::menu_button_image(BUTTON_PX, over_button))?;
        }

        // Both hands on one monitor: the distance between them sets its width.
        if let Some(s) = scene.scaling
            && let (Some(a), Some(b)) = (controls.state[0].ray, controls.state[1].ray)
            && let Some(p) = scene.placements.get_mut(&s.id)
        {
            let d = a.origin.distance(b.origin);
            p.width = (s.start_width * d / s.start_distance.max(0.05)).clamp(0.2, 8.0);
        }

        #[allow(clippy::needless_range_loop)] // `hand` indexes several per-hand arrays
        for hand in 0..2 {
            let mut input = controls.state[hand];
            // A hand that just touched the menu: its next pinch is the hand pulling
            // back, not a click.
            if input.is_hand
                && pokes[hand]
                    .touched_at
                    .is_some_and(|t| t.elapsed() < AFTER_TOUCH)
            {
                input.select.pressed = false;
            }
            if input.select.pressed {
                let mut clicked = false;
                match picks[hand] {
                    Some((Target::Panel, hit)) => {
                        panel.hover(Some(panel_point(&hit)));
                        if let Some(action) = panel.hovered_action() {
                            clicked = true;
                            controls.pulse(&session, hand);
                            handle_action(&mut scene, action, &mut passthrough, &mut controls);
                        }
                    }
                    Some((Target::MenuButton, _)) => {
                        clicked = true;
                        controls.pulse(&session, hand);
                        if scene.panel_open {
                            scene.panel_open = false;
                        } else {
                            scene.open_panel(head, forward, hands_in_view);
                        }
                    }
                    _ => {}
                }
                // Hands drag with pinch-and-hold, like the Quest's own windows.
                if input.is_hand && !clicked && picks[hand].is_some() {
                    hold_since[hand] = Some(Instant::now());
                }
            }
            if input.select.released {
                hold_since[hand] = None;
            }
            let (start_grab, end_grab) = if input.is_hand {
                let held = input.select.down
                    && hold_since[hand].is_some_and(|t| t.elapsed() >= HOLD_TO_DRAG);
                (held && scene.grabs[hand].is_none(), input.select.released)
            } else {
                (input.grab.pressed, input.grab.released)
            };

            if end_grab && scene.scaling.is_some_and(|s| s.hand == hand) {
                scene.scaling = None;
                layout::save(&scene.layout_path, &scene.placements);
            }
            if end_grab && let Some(released) = scene.grabs[hand].take() {
                // Letting go with the first hand ends resizing too.
                scene.scaling = None;
                if released.id == KEYBOARD_ID {
                    if let Some(k) = scene.keyboard_place {
                        layout::save(&scene.keyboard_path, &HashMap::from([(0, k)]));
                    }
                } else if !scene.settings.cinema {
                    layout::save(&scene.layout_path, &scene.placements);
                    // Dropped somewhere else: KDE's arrangement follows what the eyes see.
                    if released.id != PANEL_ID && released.id != MIRROR_ID {
                        scene.send_layout();
                    }
                }
            }
            let Some(ray) = input.ray else { continue };
            if let Some(mut grab) = scene.grabs[hand] {
                let target = match grab.id {
                    PANEL_ID => Some(&mut scene.panel_place),
                    KEYBOARD_ID => scene.keyboard_place.as_mut(),
                    id => scene.placements.get_mut(&id),
                };
                if let Some(p) = target {
                    grab.update(p, ray, head, input.stick, dt);
                    if grab.id == KEYBOARD_ID {
                        // It follows the hand but stays flat, like the desk.
                        p.rotation = lying_flat(p.position, head);
                    }
                    scene.grabs[hand] = Some(grab);
                }
            } else if start_grab {
                hold_since[hand] = None;
                let other = scene.grabs[1 - hand].map(|g| g.id);
                let grabbed = match picks[hand] {
                    Some((Target::Panel, hit)) if other != Some(PANEL_ID) => {
                        Some(Grab::start(PANEL_ID, hit.t, hit.local, &scene.panel_place))
                    }
                    Some((Target::Keyboard, hit)) if other != Some(KEYBOARD_ID) => scene
                        .keyboard_place
                        .map(|k| Grab::start(KEYBOARD_ID, hit.t, hit.local, &k)),
                    // The other hand already holds it: resize with both.
                    Some((Target::Monitor(id), _)) if other == Some(id) => {
                        if let (Some(a), Some(b), Some(p)) = (
                            controls.state[0].ray,
                            controls.state[1].ray,
                            scene.placements.get(&id),
                        ) {
                            scene.scaling = Some(Scaling {
                                id,
                                hand,
                                start_distance: a.origin.distance(b.origin),
                                start_width: p.width,
                            });
                            controls.pulse(&session, hand);
                        }
                        None
                    }
                    Some((Target::Monitor(id), hit)) if other != Some(id) => {
                        Some(Grab::start(id, hit.t, hit.local, &scene.placements[&id]))
                    }
                    _ => None,
                }
                .map(|g| {
                    if input.is_hand {
                        g.with_reach(ray.origin, head)
                    } else {
                        g
                    }
                });
                if grabbed.is_some() {
                    controls.pulse(&session, hand);
                    scene.grabs[hand] = grabbed;
                }
            }
        }

        // Menu contents.
        if scene.panel_open && !panel_was_open {
            // The user may have copied a picture onto the headset meanwhile.
            custom_pano = custom_panorama_path(external_dir.as_deref()).is_some();
        }
        crate::i18n::set_current(scene.lang());
        panel_was_open = scene.panel_open;
        if scene.panel_open {
            let ui_state = scene.ui_state(
                passthrough.is_some(),
                passthrough.as_ref().is_some_and(|p| p.window_supported()),
                exts.khr_composition_layer_equirect2,
                custom_pano,
            );
            panel.set_state(&ui_state);
            if let Some(pixels) = panel.render_if_dirty() {
                panel_layer.upload(&gl, pixels)?;
            }
        }

        // Hands and rays: only rendered when there is something to show.
        let mut spheres: Vec<Sphere> = Vec::new();
        for j in joints.iter().flatten() {
            overlay::hand_spheres(j, &mut spheres);
        }
        for (input, pick) in controls.state.iter().zip(&picks) {
            if let Some(ray) = input.ray {
                let length = pick.map_or(1.2, |(_, h)| h.t).min(4.0);
                overlay::ray_spheres(ray.origin, ray.dir, length, &mut spheres);
                if !input.is_hand {
                    // Where the controller is, since it is not visible otherwise.
                    spheres.push(Sphere {
                        center: ray.origin,
                        radius: 0.012,
                        color: [0.30, 0.32, 0.38, 1.0],
                    });
                }
            }
        }
        let views = if spheres.is_empty() {
            None
        } else {
            session
                .locate_views(xr::ViewConfigurationType::PRIMARY_STEREO, time, &local)
                .ok()
                .map(|(_, v)| v)
        };
        if let Some(ov) = &mut overlay {
            ov.render(&gl, views.as_deref().unwrap_or(&[]), &mut spheres)?;
        }
        let projection_views = overlay
            .as_ref()
            .zip(views.as_ref())
            .and_then(|(o, v)| o.layer_views(v));
        let hands_layer = projection_views
            .as_ref()
            .map(|v| overlay::projection_layer(&local, v));

        // ---- Layers, back to front ----
        // Desktop text is denser than the display, so ask the compositor to supersample
        // and sharpen instead of shimmering through plain bilinear sampling.
        let quality = xr::sys::CompositionLayerSettingsFB {
            ty: xr::sys::CompositionLayerSettingsFB::TYPE,
            next: ptr::null(),
            layer_flags: xr::sys::CompositionLayerSettingsFlagsFB::QUALITY_SUPER_SAMPLING
                | xr::sys::CompositionLayerSettingsFlagsFB::QUALITY_SHARPENING,
        };
        let quality_next: *const std::ffi::c_void = if exts.fb_composition_layer_settings {
            (&quality as *const xr::sys::CompositionLayerSettingsFB).cast()
        } else {
            ptr::null()
        };
        // Android surfaces store rows top-down while the compositor samples them
        // GL-style (bottom-up), which shows the desktop upside down.
        let flip = xr::sys::CompositionLayerImageLayoutFB {
            ty: xr::sys::CompositionLayerImageLayoutFB::TYPE,
            next: quality_next.cast_mut(),
            flags: xr::sys::CompositionLayerImageLayoutFlagsFB::VERTICAL_FLIP,
        };
        let video_next = if exts.fb_composition_layer_image_layout {
            (&flip as *const xr::sys::CompositionLayerImageLayoutFB).cast()
        } else {
            quality_next
        };

        // The keyboard window: placed in front of the user at desk height the first
        // time, then wherever it was moved.
        if let Some(pt) = passthrough.as_mut().filter(|pt| pt.window_supported()) {
            let place = scene.settings.keyboard_window.then(|| {
                *scene.keyboard_place.get_or_insert_with(|| {
                    let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(-Vec3::Z);
                    let position = head + flat * 0.42 - Vec3::Y * 0.42;
                    Placement {
                        position,
                        rotation: lying_flat(position, head),
                        width: 0.5,
                    }
                })
            });
            let shown = place.map(|p| (posef(&p), p.width, p.width * KEYBOARD_ASPECT));
            if let Err(e) = pt.set_window(&local, time, shown) {
                warn!("keyboard window: {e:#}");
                scene.settings.keyboard_window = false;
            }
        }
        let passthrough_layer = |handle: Option<xr::sys::PassthroughLayerFB>| {
            handle.map(|layer_handle| xr::sys::CompositionLayerPassthroughFB {
                ty: xr::sys::CompositionLayerPassthroughFB::TYPE,
                next: ptr::null(),
                flags: xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA,
                space: xr::sys::Space::NULL,
                layer_handle,
            })
        };
        let keyboard_layer =
            passthrough_layer(passthrough.as_ref().and_then(|pt| pt.window_layer()));
        let passthrough_layer =
            passthrough_layer(passthrough.as_ref().and_then(|pt| pt.room_layer()));
        let pano = pano_layer
            .as_ref()
            .filter(|(b, _)| *b == scene.settings.background)
            .map(|(_, l)| xr::sys::CompositionLayerEquirect2KHR {
                ty: xr::sys::CompositionLayerEquirect2KHR::TYPE,
                next: ptr::null(),
                layer_flags: xr::CompositionLayerFlags::EMPTY,
                space: local.as_raw(),
                eye_visibility: xr::EyeVisibility::BOTH,
                sub_image: l.sub_image(),
                pose: xr::Posef::IDENTITY,
                radius: 0.0,
                central_horizontal_angle: std::f32::consts::TAU,
                upper_vertical_angle: std::f32::consts::FRAC_PI_2,
                lower_vertical_angle: -std::f32::consts::FRAC_PI_2,
            });

        // Far monitors first so nearer (and grabbed) ones draw on top.
        let grabbed: Vec<u8> = scene.grabs.iter().flatten().map(|g| g.id).collect();
        scene.animate(dt, &grabbed);
        // Keep KDE's arrangement in step once nothing is being moved.
        if grabbed.is_empty() && scene.scaling.is_none() && !scene.relayout_pending() {
            scene.send_layout();
        }
        let mut order: Vec<(f32, u8)> = scene
            .monitors
            .keys()
            .filter_map(|id| {
                let d = scene.shown.get(id)?.position.distance(head);
                Some((if grabbed.contains(id) { -1.0 } else { d }, *id))
            })
            .collect();
        order.sort_by(|a, b| b.0.total_cmp(&a.0));
        let curve = scene.curve_radius();
        // Focus mode: monitors without the mouse fade to DIMMED (colour scale on the
        // compositor layer, no extra rendering).
        const DIMMED: f32 = 0.4;
        for id in scene.monitors.keys() {
            let target = match scene.focus {
                Some(f) if scene.settings.focus && f != *id && !grabbed.contains(id) => DIMMED,
                _ => 1.0,
            };
            let b = scene.brightness.entry(*id).or_insert(1.0);
            let step = dt * 5.0;
            *b = if *b < target {
                (*b + step).min(target)
            } else {
                (*b - step).max(target)
            };
        }
        let dims: Vec<xr::sys::CompositionLayerColorScaleBiasKHR> = order
            .iter()
            .map(|(_, id)| {
                // Fading in: premultiplied, so colour and alpha scale together.
                let t = scene.appear.get(id).copied().unwrap_or(1.0);
                let t = t * t * (3.0 - 2.0 * t);
                let b = scene.brightness.get(id).copied().unwrap_or(1.0) * t;
                xr::sys::CompositionLayerColorScaleBiasKHR {
                    ty: xr::sys::CompositionLayerColorScaleBiasKHR::TYPE,
                    next: video_next,
                    color_scale: xr::Color4f {
                        r: b,
                        g: b,
                        b,
                        a: t,
                    },
                    color_bias: xr::Color4f {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 0.0,
                    },
                }
            })
            .collect();
        let mut quads = Vec::new();
        let mut cylinders = Vec::new();
        for (i, (_, id)) in order.iter().enumerate() {
            let scale = dims[i].color_scale;
            let fading = exts.khr_composition_layer_color_scale_bias && scale.a < 1.0;
            let layer_flags = if fading {
                xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA
            } else {
                xr::CompositionLayerFlags::EMPTY
            };
            let next = if exts.khr_composition_layer_color_scale_bias && scale.r < 1.0 {
                (&dims[i] as *const xr::sys::CompositionLayerColorScaleBiasKHR).cast()
            } else {
                video_next
            };
            let m = &scene.monitors[id];
            let p = &scene.shown[id];
            let height = p.width * m.aspect();
            let sub_image = xr::sys::SwapchainSubImage {
                swapchain: m.swapchain.as_raw(),
                image_rect: xr::Rect2Di {
                    offset: xr::Offset2Di { x: 0, y: 0 },
                    extent: xr::Extent2Di {
                        width: m.width_px as i32,
                        height: m.height_px as i32,
                    },
                },
                image_array_index: 0,
            };
            if let Some(radius) = curve {
                let c = Cylinder::new(p, radius);
                cylinders.push(xr::sys::CompositionLayerCylinderKHR {
                    ty: xr::sys::CompositionLayerCylinderKHR::TYPE,
                    next,
                    layer_flags,
                    space: local.as_raw(),
                    eye_visibility: xr::EyeVisibility::BOTH,
                    sub_image,
                    pose: pose(c.center, p.rotation),
                    radius: c.radius,
                    central_angle: c.central_angle,
                    aspect_ratio: c.arc_length() / height,
                });
            } else {
                quads.push(xr::sys::CompositionLayerQuad {
                    ty: xr::sys::CompositionLayerQuad::TYPE,
                    next,
                    layer_flags,
                    space: local.as_raw(),
                    eye_visibility: xr::EyeVisibility::BOTH,
                    sub_image,
                    pose: posef(p),
                    size: xr::Extent2Df {
                        width: p.width,
                        height,
                    },
                });
            }
        }

        let button = scene
            .button_place
            .map(|b| button_layer.quad(&local, posef(&b), BUTTON_SIZE, BUTTON_SIZE));
        let panel_quad = (scene.panel_open && panel_layer.ready()).then(|| {
            panel_layer.quad(
                &local,
                posef(&scene.panel_place),
                PANEL_WIDTH,
                panel_height(),
            )
        });
        let mut cursors: Vec<xr::sys::CompositionLayerQuad> = picks
            .iter()
            .flatten()
            .map(|(_, hit)| {
                let size = 0.011 * hit.t.clamp(0.3, 4.0);
                let at = hit.point + hit.normal * 0.004;
                cursor_layer.quad(
                    &local,
                    pose(at, layout::facing(at, at + hit.normal)),
                    size,
                    size,
                )
            })
            .collect();
        // Under a fingertip near the menu: a dot that shrinks as the finger gets closer.
        for (at, z) in poke_hover.iter().flatten() {
            let place = &scene.panel_place;
            let normal = place.rotation * Vec3::Z;
            let point =
                place.position + place.rotation * Vec3::new(at.x, at.y, 0.0) + normal * 0.002;
            let size = 0.006 + z.max(0.0) * 0.15;
            cursors.push(cursor_layer.quad(
                &local,
                pose(point, layout::facing(point, point + normal)),
                size,
                size,
            ));
        }

        let mut layers: Vec<&xr::CompositionLayerBase<Gfx>> = Vec::with_capacity(16);
        if let Some(l) = &passthrough_layer {
            layers.push(as_layer(l));
        } else if let Some(l) = &pano {
            layers.push(as_layer(l));
        }
        layers.extend(keyboard_layer.iter().map(as_layer));
        layers.extend(cylinders.iter().map(as_layer));
        layers.extend(quads.iter().map(as_layer));
        layers.extend(button.iter().map(as_layer));
        layers.extend(panel_quad.iter().map(as_layer));
        layers.extend(hands_layer.iter().map(as_layer));
        layers.extend(cursors.iter().map(as_layer));
        // The compositor supports 16 layers at most.
        layers.truncate(16);
        frame_stream.end(time, xr::EnvironmentBlendMode::OPAQUE, &layers)?;
    }

    info!("exiting");
    drop(scene);
    Ok(())
}

fn handle_net_event(
    scene: &mut Scene,
    session: &xr::Session<Gfx>,
    vm: &jni::JavaVM,
    event: NetEvent,
) {
    match event {
        NetEvent::Connected {
            addr,
            name,
            feedback,
            language,
        } => {
            info!("connected to {addr} ({name:?}), language {language:?}");
            if language.is_some() && language != scene.settings.pc_language {
                scene.settings.pc_language = language;
                scene.settings.save(&scene.settings_path);
            }
            let usb = scene.lang().t("ПК (USB)", "PC (USB)");
            scene.host = Some(name.unwrap_or_else(|| {
                if addr.ip().is_loopback() {
                    usb.into()
                } else {
                    addr.ip().to_string()
                }
            }));
            scene.feedback = Some(feedback);
            scene.sent_layout = None;
            scene.update_needed = None;
            // The PC follows the headset's microphone switch.
            scene.send(ClientMsg::SetMic {
                enabled: scene.settings.mic,
            });
            if !scene.worn {
                scene.tell_presence();
            }
            scene.offline_since = None;
            scene.ip_input = None;
            scene.unreachable = None;
        }
        NetEvent::Hosts(hosts) => scene.found = hosts,
        NetEvent::Unreachable(name) => scene.unreachable = name,
        NetEvent::Pairing { host, code } => {
            let failed = scene.pairing.as_ref().is_some_and(|p| p.failed);
            scene.pairing = Some(ui::PairingView {
                host,
                code,
                typed: String::new(),
                failed,
            });
        }
        NetEvent::PairingFailed => {
            if let Some(p) = &mut scene.pairing {
                p.failed = true;
                p.typed.clear();
            }
        }
        NetEvent::Paired => scene.pairing = None,
        NetEvent::PairOffer { host, failed } => {
            scene.pairing = None;
            scene.pair_offer = Some((host, failed));
        }
        NetEvent::Incompatible { pc_newer, pc_app } => {
            warn!("the PC runs Prostir {pc_app}: versions differ");
            scene.update_needed = Some((pc_newer, pc_app));
        }
        NetEvent::Disconnected => {
            scene.offline_since = Some(Instant::now());
            scene.pairing = None;
            scene.host = None;
            scene.feedback = None;
        }
        NetEvent::PrimaryChanged(id) => {
            info!("primary monitor: Prostir-{}", id + 1);
            scene.primary = Some(id);
            if scene.relayout_pending() {
                scene.arrange_all();
            }
        }
        NetEvent::MirrorWanted(enabled) => scene.mirror_wanted = enabled,
        NetEvent::AudioWanted(enabled) => scene.audio_wanted = enabled,
        NetEvent::Focus(id) => scene.focus = id,
        NetEvent::Order(ids) => {
            scene.order = ids;
            if scene.relayout_pending() {
                scene.arrange_all();
            }
        }
        NetEvent::HostError(text) => scene.show_error(text),
        NetEvent::StreamStart {
            id,
            width,
            height,
            codec,
            frames,
            feedback,
        } => {
            scene.monitors.remove(&id);
            match xrutil::create_surface_swapchain(session, vm, width, height) {
                Ok((swapchain, window)) => {
                    let stop = Arc::new(AtomicBool::new(false));
                    let decoder = decoder::spawn(
                        id,
                        codec,
                        width,
                        height,
                        window,
                        frames,
                        feedback,
                        stop.clone(),
                    );
                    scene.monitors.insert(
                        id,
                        Monitor {
                            width_px: width,
                            height_px: height,
                            decoder: Some(decoder),
                            stop,
                            swapchain,
                        },
                    );
                    if scene.relayout_pending() {
                        scene.arrange_all();
                    } else if !scene.placements.contains_key(&id) && !scene.settings.cinema {
                        if id == MIRROR_ID {
                            // Only the mirror is new: put it under the primary monitor.
                            scene.place_mirror();
                        } else {
                            // A new monitor: re-arrange so it joins the row seamlessly.
                            scene.arrange_all();
                        }
                    }
                }
                Err(e) => error!("monitor {id}: {e:#}"),
            }
        }
        NetEvent::StreamEnd { id, removed } => {
            scene.monitors.remove(&id);
            for g in &mut scene.grabs {
                if g.is_some_and(|g| g.id == id) {
                    *g = None;
                }
            }
            if removed && id != MIRROR_ID {
                scene.placements.remove(&id);
                scene.arrange_all();
            }
        }
    }
}

/// Passthrough cameras on only for the "room" background.
fn set_passthrough(scene: &mut Scene, passthrough: &mut Option<passthrough::Passthrough>) {
    if let Some(pt) = passthrough
        && let Err(e) = pt.set_room(scene.settings.background == Background::Room)
    {
        let what = scene.lang().t("Камери недоступні", "Cameras unavailable");
        scene.show_error(format!("{what}: {e:#}"));
    }
}

fn handle_action(
    scene: &mut Scene,
    action: Action,
    passthrough: &mut Option<passthrough::Passthrough>,
    controls: &mut Controls,
) {
    let settings_path: &Path = &scene.settings_path.clone();
    // Changing the setup by hand leaves the profile it came from.
    if matches!(
        action,
        Action::Add { .. }
            | Action::Remove(_)
            | Action::MakePrimary(_)
            | Action::Move { .. }
            | Action::SetMirror(_)
            | Action::Preset(_)
            | Action::Curve(_)
            | Action::Background(_)
            | Action::Cinema(_)
            | Action::CinemaScreen(_)
    ) {
        scene.active_profile = None;
    }
    match action {
        Action::Add { width, height } => scene.send(ClientMsg::AddMonitor { width, height }),
        Action::Remove(id) => {
            let confirmed = scene
                .confirm_remove
                .is_some_and(|(c, at)| c == id && at.elapsed() < CONFIRM_FOR);
            if confirmed {
                scene.confirm_remove = None;
                scene.send(ClientMsg::RemoveMonitor { id });
            } else {
                scene.confirm_remove = Some((id, Instant::now()));
            }
        }
        Action::Tab(tab) => scene.tab = tab,
        Action::Recenter => {
            let (head, forward) = scene.head;
            let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(-Vec3::Z);
            scene.center = (head, (-flat.x).atan2(-flat.z));
            scene.arrange_all();
            // The menu comes along, in front again.
            let touch = scene.hands_in_view;
            scene.open_panel(head, forward, touch);
        }
        Action::IntroDone => {
            scene.settings.intro_seen = true;
            scene.settings.save(settings_path);
        }
        Action::Language(language) => {
            scene.settings.language = language;
            scene.settings.save(settings_path);
        }
        Action::CinemaScreen(i) => {
            scene.settings.cinema_screen = Some(i);
            scene.settings.save(settings_path);
            if scene.settings.cinema {
                scene.arrange_all();
            }
        }
        Action::Cinema(on) => {
            scene.settings.cinema = on;
            scene.settings.save(settings_path);
            if on {
                scene.arrange_all();
            } else {
                // Back to the desk layout as it was saved.
                scene.placements = layout::load(&scene.layout_path);
                if scene
                    .monitors
                    .keys()
                    .any(|id| !scene.placements.contains_key(id))
                {
                    scene.arrange_all();
                }
            }
        }
        Action::KeyboardWindow(on) => {
            scene.settings.keyboard_window = on;
            scene.settings.save(settings_path);
        }
        Action::Focus(on) => {
            scene.settings.focus = on;
            scene.settings.save(settings_path);
        }
        Action::Hands(mode) => {
            scene.settings.hands = mode;
            scene.settings.save(settings_path);
            controls.hands_enabled = mode == HandMode::Control;
        }
        Action::MakePrimary(id) => scene.send(ClientMsg::SetPrimary { id }),
        Action::Move { id, delta } => {
            let mut ids: Vec<u8> = scene.quest_sizes().iter().map(|m| m.0).collect();
            if let Some(i) = ids.iter().position(|x| *x == id) {
                let j = (i as i32 + delta).clamp(0, ids.len() as i32 - 1) as usize;
                ids.swap(i, j);
                scene.order = ids;
                scene.arrange_all();
            }
        }
        Action::SetMic(enabled) => {
            scene.mic_wanted = enabled;
            scene.settings.mic = enabled;
            scene.settings.save(&scene.settings_path);
            crate::mic::set_allowed(enabled);
            scene.send(ClientMsg::SetMic { enabled });
        }
        Action::SetAudio(enabled) => {
            scene.audio_wanted = enabled;
            scene.send(ClientMsg::SetAudio { enabled });
        }
        Action::SetMirror(enabled) => {
            scene.mirror_wanted = enabled;
            scene.send(ClientMsg::SetMirror { enabled });
        }
        Action::Preset(preset) => {
            scene.settings.preset = preset;
            scene.settings.save(settings_path);
            scene.arrange_all();
        }
        Action::Curve(curve) => {
            scene.settings.curve = curve;
            scene.settings.save(settings_path);
            // Flat it hangs tilted, curved it continues the cylinder.
            if !scene.settings.cinema {
                scene.place_mirror();
            }
        }
        Action::Background(background) => {
            scene.settings.background = background;
            scene.settings.save(settings_path);
            set_passthrough(scene, passthrough);
        }
        Action::ApplyProfile(i) => {
            // Never saved yet: the setup the user built by hand becomes "Work", so
            // switching away and back returns to it instead of the starting template.
            if !scene.profiles_path.exists() && !scene.quest_sizes().is_empty() {
                scene.profiles[0] = scene.current_profile();
                profiles::save(&scene.profiles_path, &scene.profiles);
            }
            let p = scene.profiles[i].clone();
            scene.send(ClientMsg::SetMonitors {
                sizes: p.sizes.clone(),
                primary: p.primary,
            });
            if p.mirror != scene.mirror_wanted {
                scene.mirror_wanted = p.mirror;
                scene.send(ClientMsg::SetMirror { enabled: p.mirror });
            }
            let s = &mut scene.settings;
            s.preset = p.settings.preset;
            s.curve = p.settings.curve;
            s.background = p.settings.background;
            s.cinema = p.settings.cinema;
            s.cinema_screen = p.settings.cinema_screen;
            scene.settings.save(settings_path);
            set_passthrough(scene, passthrough);
            scene.active_profile = Some(i);
            scene.relayout_until = Some(Instant::now() + Duration::from_secs(8));
            scene.arrange_all();
        }
        Action::SaveProfile(i) => {
            scene.profiles[i] = scene.current_profile();
            profiles::save(&scene.profiles_path, &scene.profiles);
            scene.active_profile = Some(i);
        }
        Action::ConnectTo(i) => {
            if let Some(h) = scene.found.get(i) {
                scene.steering.connect_to(h.addr);
            }
        }
        Action::ShowKeypad => scene.ip_input = Some(String::new()),
        Action::Key(c) if scene.pairing.is_some() => {
            if let Some(p) = &mut scene.pairing
                && p.typed.len() < 6
                && c.is_ascii_digit()
            {
                p.typed.push(c);
            }
        }
        Action::Backspace if scene.pairing.is_some() => {
            if let Some(p) = &mut scene.pairing {
                p.typed.pop();
            }
        }
        Action::ConnectTyped if scene.pairing.is_some() => {
            if let Some(p) = &mut scene.pairing {
                scene.steering.submit_code(std::mem::take(&mut p.typed));
                p.failed = false;
            }
        }
        Action::PairAnswer(pair) => {
            scene.steering.answer_offer(pair);
            scene.pair_offer = None;
        }
        Action::PairOtherWay => {
            scene.steering.switch_pairing_side();
            scene.pairing = None;
        }
        Action::Key(c) => {
            if let Some(t) = &mut scene.ip_input
                && t.len() < 21
            {
                t.push(c);
            }
        }
        Action::Backspace => {
            if let Some(t) = &mut scene.ip_input {
                t.pop();
            }
        }
        Action::ConnectTyped => {
            let typed = scene.ip_input.clone().unwrap_or_default();
            match net::parse_addr(&typed) {
                Some(addr) => {
                    scene.steering.connect_to(addr);
                    scene.ip_input = None;
                }
                None => {
                    let what = scene.lang().t("не IP-адреса", "is not an IP address");
                    scene.show_error(format!("«{typed}» — {what}"))
                }
            }
        }
        Action::CancelKeypad => scene.ip_input = None,
        Action::ChangeHost => scene.steering.change_host(),
        Action::Close => {
            scene.panel_open = false;
            // Closing the tips counts as read, or they would open again right away.
            if !scene.settings.intro_seen {
                scene.settings.intro_seen = true;
                scene.settings.save(settings_path);
            }
        }
    }
}
