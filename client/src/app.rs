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
use prostir_proto::request::{Outcome, Primary, Request};
use prostir_proto::state::{
    self, Battery, DisplayId, DisplayKind, HeadsetCaps, HeadsetState, Mode, Prefs, Presence, ReqId,
    caps,
};
use prostir_proto::{ClientMsg, Codec};

use crate::i18n::{Lang, Language};
use crate::input::Controls;
use crate::layout::{self, Cylinder, Grab, Held, Hit, Placement, Ray, ScreenId};
use crate::net::{Feedback, NetEvent};
use crate::overlay::{self, Overlay, Sphere};
use crate::profiles::{self, Profile};
use crate::settings::{Background, HandMode, Settings};
use crate::ui::{self, Action, MonitorEntry, Panel, Tab, UiState};
use crate::xrutil::{self, Gfx, ImageLayer, as_layer, pose, posef, vec3};
use crate::{background, decoder, egl, net, passthrough};

/// The keyboard window's depth as a share of its width (a keyboard's shape).
const KEYBOARD_ASPECT: f32 = 0.44;
const PANEL_WIDTH: f32 = 0.62;
const BUTTON_SIZE: f32 = 0.075;
const CURSOR_PX: u32 = 64;
const BUTTON_PX: u32 = 128;
/// Toast texture and its size in the room.
const TOAST_PX: (u32, u32) = (1024, 132);
const TOAST_WIDTH: f32 = 0.46;
/// How long a toast stays.
const TOAST_FOR: Duration = Duration::from_secs(4);
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
    id: ScreenId,
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

/// How long the ring of a touch spreads.
const TOUCH_RING: Duration = Duration::from_millis(350);

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

/// 90 Hz (or the nearest below), or 72 Hz to save battery.
fn set_refresh_rate(session: &xr::Session<Gfx>, saver: bool) {
    let limit = if saver { 72.0 } else { 90.0 };
    let rates = session
        .enumerate_display_refresh_rates()
        .unwrap_or_default();
    info!("display rates offered: {rates:?}");
    let best = rates.into_iter().filter(|&r| r <= limit).reduce(f32::max);
    if let Some(rate) = best {
        info!("display at {rate} Hz");
        let _ = session.request_display_refresh_rate(rate);
    }
}

/// The headset's local time, broken down.
fn local_time() -> Option<libc::tm> {
    // SAFETY: plain libc time calls on a zeroed `tm`.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&now, &mut tm).is_null()).then_some(tm)
    }
}

/// The time as `14:05` or `2:05 PM`.
fn clock_text(tm: &libc::tm, twelve: bool) -> String {
    if !twelve {
        return format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
    }
    let hour = match tm.tm_hour % 12 {
        0 => 12,
        h => h,
    };
    let half = if tm.tm_hour < 12 { "AM" } else { "PM" };
    format!("{hour}:{:02} {half}", tm.tm_min)
}

/// The date as `Ср, 18 лист.`, `Wed, Nov 18` or `18.11.2026`.
fn date_text(tm: &libc::tm, style: crate::settings::DateStyle, lang: Lang) -> String {
    use crate::settings::DateStyle;
    let (day, month) = (
        tm.tm_wday.clamp(0, 6) as usize,
        tm.tm_mon.clamp(0, 11) as usize,
    );
    let style = match (style, lang) {
        (DateStyle::LikeMenu, Lang::Uk) => DateStyle::Ukrainian,
        (DateStyle::LikeMenu, Lang::En) => DateStyle::English,
        (s, _) => s,
    };
    match style {
        DateStyle::English => {
            const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            format!("{}, {} {}", DAYS[day], MONTHS[month], tm.tm_mday)
        }
        DateStyle::Numbers => format!(
            "{:02}.{:02}.{}",
            tm.tm_mday,
            tm.tm_mon + 1,
            tm.tm_year + 1900
        ),
        _ => {
            const DAYS: [&str; 7] = ["Нд", "Пн", "Вт", "Ср", "Чт", "Пт", "Сб"];
            const MONTHS: [&str; 12] = [
                "січ.",
                "лют.",
                "бер.",
                "квіт.",
                "трав.",
                "черв.",
                "лип.",
                "серп.",
                "вер.",
                "жовт.",
                "лист.",
                "груд.",
            ];
            format!("{}, {} {}", DAYS[day], tm.tm_mday, MONTHS[month])
        }
    }
}

/// The menu's size factor (the "Large" menu setting), as `f32` bits.
static MENU_SCALE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(1.0f32.to_bits());

fn set_menu_scale(large: bool) {
    let scale: f32 = if large { 1.3 } else { 1.0 };
    MENU_SCALE.store(scale.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

fn panel_width() -> f32 {
    PANEL_WIDTH * f32::from_bits(MENU_SCALE.load(std::sync::atomic::Ordering::Relaxed))
}

fn panel_height() -> f32 {
    panel_width() * ui::HEIGHT as f32 / ui::WIDTH as f32
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
    Monitor(ScreenId),
    /// "Attach" under a detached window: back onto the monitors.
    Attach(ScreenId),
    /// The menu by the left wrist.
    Wrist,
    Keyboard,
}

/// The "Attach" button under a pointed-at detached window: size in the room and in
/// pixels, how far below the window, and how long it stays after the pointer left.
const ATTACH_WIDTH: f32 = 0.11;
const ATTACH_PX: (u32, u32) = (352, 96);
const ATTACH_GAP: f32 = 0.03;
const ATTACH_LINGER: Duration = Duration::from_millis(1200);

fn attach_height() -> f32 {
    ATTACH_WIDTH * ATTACH_PX.1 as f32 / ATTACH_PX.0 as f32
}
/// Width of the wrist menu in the room.
const WRIST_MENU_WIDTH: f32 = 0.13;
/// How long an open palm faces the face before its menu appears.
const PALM_DWELL: Duration = Duration::from_millis(250);
/// How long the gaze rests on another monitor before the mouse goes there.
const GAZE_DWELL: Duration = Duration::from_millis(600);
/// How long the "where is the mouse" rings show.
const FINDER_FOR: Duration = Duration::from_millis(1600);

fn wrist_menu_height() -> f32 {
    WRIST_MENU_WIDTH * ui::WRIST_HEIGHT as f32 / ui::WRIST_WIDTH as f32
}

/// Where the palm menu goes: lying on the palm (a little above it, its top towards the
/// fingers), while an open palm is turned to the face. It takes a quarter of a second
/// of that to appear, so hands passing by (typing, reaching) do not bring it up.
/// `since`: when the palm started facing the face.
fn palm_menu_place(
    joints: Option<&[(Vec3, f32); 26]>,
    right_hand: bool,
    head: Vec3,
    previous: Option<Placement>,
    since: &mut Option<Instant>,
) -> Option<Placement> {
    let Some(j) = joints else {
        *since = None;
        return None;
    };
    let (palm, wrist) = (j[0].0, j[1].0);
    // Knuckles, a hand's width apart: a steady palm normal. For the left hand it is
    // little × index from the wrist (checked in the headset: the other order points
    // out of the back of the hand); the right hand is its mirror image.
    let (index, middle, little, middle_tip) = (j[7].0, j[12].0, j[22].0, j[15].0);
    let mut normal = (little - wrist).cross(index - wrist).normalize_or_zero();
    if right_hand {
        normal = -normal;
    }
    let facing = normal.dot((head - palm).normalize_or_zero());
    let open = middle_tip.distance(palm) > 0.07;
    let needed = if previous.is_some() { 0.55 } else { 0.8 };
    if facing < needed || !open {
        *since = None;
        return None;
    }
    if previous.is_none() && since.get_or_insert_with(Instant::now).elapsed() < PALM_DWELL {
        return None;
    }
    let up = (middle - wrist)
        .reject_from_normalized(normal)
        .normalize_or(Vec3::Y);
    let rotation = glam::Quat::from_mat3(&glam::Mat3::from_cols(up.cross(normal), up, normal));
    let mut at = palm + normal * 0.03;
    let mut rotation = rotation.normalize();
    // Hands shake a little: the menu follows smoothly.
    if let Some(p) = previous {
        at = p.position.lerp(at, 0.35);
        rotation = p.rotation.slerp(rotation, 0.35);
    }
    Some(Placement {
        position: at,
        rotation,
        width: WRIST_MENU_WIDTH,
    })
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
    monitors: HashMap<ScreenId, Monitor>,
    /// Left-to-right order of the Quest monitors, shared with the host and KDE.
    order: Vec<ScreenId>,
    placements: HashMap<ScreenId, Placement>,
    primary: Option<ScreenId>,
    mirror_wanted: bool,
    audio_wanted: bool,
    mic_wanted: bool,
    /// The monitor the mouse is on (from the PC).
    focus: Option<ScreenId>,
    /// Current brightness of each monitor, easing towards 1 or the dimmed level.
    brightness: HashMap<ScreenId, f32>,
    /// Resizing a monitor with both hands.
    scaling: Option<Scaling>,
    /// Where each monitor is drawn: follows `placements`, easing into new spots.
    shown: HashMap<ScreenId, Placement>,
    /// How far each monitor has faded in (0 to 1) since it appeared.
    appear: HashMap<ScreenId, f32>,
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
    /// How the connected PC encodes, and what it can encode with.
    pc: Option<state::Encoding>,
    /// The connected PC's displays, as it last listed them in this session.
    pc_displays: Option<state::Displays>,
    /// The number of the last request to the PC in this session.
    last_req: u32,
    /// An unknown PC waiting for "Pair" or "Not now" (survives the disconnect that
    /// follows the question).
    pair_offer: Option<(String, crate::ui::OfferWhy)>,
    steering: net::Steering,
    /// When the connection was lost (or the app started without one).
    offline_since: Option<Instant>,
    feedback: Option<Feedback>,
    error: Option<(String, Instant)>,
    /// "×" pressed once on this monitor; a second press within a few seconds removes it.
    confirm_remove: Option<(ScreenId, Instant)>,
    /// "Save current" pressed once for this profile.
    confirm_save: Option<(usize, Instant)>,
    /// Paired PCs (key, name), read from the trust file when the menu opens.
    paired: Vec<([u8; 32], String)>,
    trust_file: PathBuf,
    confirm_forget: Option<(usize, Instant)>,
    /// Menu page shown.
    tab: Tab,
    /// Follow mode is moving the monitors right now.
    following: bool,
    /// Follow mode waits for the head to come back first: a monitor was just placed
    /// with the head turned, and the arrangement must not swing after it.
    follow_hold: bool,
    /// The detached window whose "Attach" button shows, and when it was pointed at.
    attach_for: Option<(ScreenId, Instant)>,
    /// The wrist menu, while the left palm is turned to the face.
    wrist: Option<Placement>,
    /// The monitor picked on the menu's map.
    selected: Option<ScreenId>,
    /// The PC can move the mouse.
    can_move_mouse: bool,
    /// The headset's battery (percent, charging), read once a minute.
    headset_battery: Option<(u8, bool)>,
    /// The monitor the sound comes from, as the PC says.
    sound_from: Option<ScreenId>,
    /// The display rate setting changed: apply it.
    refresh_due: bool,
    /// Time worn since the last rest reminder.
    worn_for: Duration,
    /// Since when the headset is off the head (for rest reminders).
    off_since: Option<Instant>,
    /// Mouse follows the gaze: the monitor looked at and since when.
    gaze_on: Option<(ScreenId, Instant)>,
    /// When the mouse was last sent somewhere by the gaze.
    gaze_moved: Option<Instant>,
    /// "Where is the mouse": monitor, point across its image, since when.
    finder: Option<(ScreenId, Vec2, Instant)>,
    /// A short message shown on its own: text, since when, where (`None` until placed).
    toast: Option<(String, Instant, Option<Placement>)>,
    /// The PC speaks another protocol: (the PC is newer, its app version).
    update_needed: Option<(bool, String)>,
    /// Where layouts are centred and which way they face (head position, yaw).
    center: (Vec3, f32),
    /// The circle around the head the arrangement stands on (it grows for many
    /// monitors, see `layout::preset_radius`).
    radius: f32,
    /// The desktop last sent to the PC.
    sent_layout: Option<layout::Desktop>,
    /// A changed desktop waiting to settle before it is sent.
    pending_layout: Option<(layout::Desktop, Instant)>,
    /// The desktop for the arrangement now, kept while nothing moves.
    desktop_memo: layout::DesktopMemo,
    /// Head position and gaze this frame.
    head: (Vec3, Vec3),
    /// The head's right, rolled and tilted with it (where the right ear is).
    head_right: Vec3,
    /// Where the eyes are, for the map in the menu: moves only when the head has moved
    /// a few centimetres (the menu redraws when its picture changes).
    view_eye: Option<Vec3>,
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
    /// After switching to a profile without places (from before 0.2.0, or a starting
    /// point), monitors come and go for a few seconds: lay them out by the preset
    /// again on each change until then.
    relayout_until: Option<Instant>,
    /// A profile's places, by its monitors' order, until the PC says which numbers
    /// they have now (its `Order`), and until when that may take.
    pending_places: Option<(Vec<Placement>, Instant)>,
    /// The desk arrangement on disk; see `save_layout`.
    desk: layout::Desk,
    /// The stream size each desk placement was made for (see `fit_to_stream`). Not
    /// kept for the cinema view.
    placed_for: HashMap<ScreenId, layout::PixelSize>,
    settings_path: PathBuf,
}

impl Scene {
    /// What display `id` of the PC is, as its list says (`None`: not listed).
    fn kind(&self, id: ScreenId) -> Option<&DisplayKind> {
        self.pc_displays
            .as_ref()?
            .get(DisplayId(id))
            .map(|d| &d.kind)
    }

    /// A virtual monitor (not a window panel, nor a display of the PC itself).
    fn is_monitor(&self, id: ScreenId) -> bool {
        matches!(self.kind(id), Some(DisplayKind::Virtual))
    }

    /// A window panel.
    fn is_window(&self, id: ScreenId) -> bool {
        matches!(self.kind(id), Some(DisplayKind::Window { .. }))
    }

    /// The display of the PC itself shown in the headset (the laptop screen), if one is.
    fn mirror_id(&self) -> Option<ScreenId> {
        self.pc_displays
            .as_ref()?
            .displays
            .iter()
            .find(|d| d.is_shown_physical())
            .map(|d| d.id.0)
    }

    /// The display of the PC itself the "laptop screen" switch shows or hides: the one
    /// shown, else the laptop's own panel, else the first one.
    fn physical_id(&self) -> Option<ScreenId> {
        let displays = &self.pc_displays.as_ref()?.displays;
        let physical = || displays.iter().filter(|d| d.is_physical());
        self.mirror_id()
            .or_else(|| {
                physical()
                    .find(|d| matches!(d.kind, DisplayKind::Physical { builtin: true, .. }))
                    .map(|d| d.id.0)
            })
            .or_else(|| physical().next().map(|d| d.id.0))
    }

    /// Quest monitors left to right: in the host's order, unknown ones at the end.
    fn quest_sizes(&self) -> Vec<(ScreenId, u32, u32)> {
        let mut ids: Vec<ScreenId> = self
            .order
            .iter()
            .copied()
            .filter(|id| self.monitors.contains_key(id))
            .collect();
        let mut rest: Vec<ScreenId> = self
            .monitors
            .keys()
            .copied()
            .filter(|id| self.is_monitor(*id) && !ids.contains(id))
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
        // Wait until every monitor the PC has is placed: a partial picture would make
        // KDE rearrange (and blink) once per arriving monitor. One whose stream could
        // not start keeps its place and does not hold the others up.
        let complete = self.order.iter().any(|id| self.monitors.contains_key(id))
            && self.order.iter().all(|id| self.placements.contains_key(id));
        if !complete {
            return;
        }
        // The laptop screen too: the PC puts it where it stands here. Monitors stay in
        // the rows the PC has until clearly moved out of them, and its desktop stays
        // while it fits as well as a new one would.
        let desktop = self.desktop();
        if desktop.rows.is_empty() || self.sent_layout.as_ref() == Some(&desktop) {
            self.pending_layout = None;
            return;
        }
        // Only an arrangement that has held for a moment: monitors arriving one by one
        // or being laid out again pass through states KDE should not be shown.
        match &self.pending_layout {
            Some((pending, since)) if *pending == desktop => {
                if since.elapsed() < Duration::from_millis(800) {
                    return;
                }
            }
            _ => {
                self.pending_layout = Some((desktop, Instant::now()));
                return;
            }
        }
        self.pending_layout = None;
        let rows = desktop.wire();
        info!(
            "KDE arrangement: {}",
            rows.iter()
                .map(|r| {
                    let ids: Vec<u32> = r.ids().map(|id| id.0).collect();
                    let centers: Vec<Option<i32>> = r.entries.iter().map(|e| e.center).collect();
                    format!("{ids:?}@{centers:?}")
                })
                .collect::<Vec<_>>()
                .join(" / ")
        );
        self.order = rows
            .iter()
            .flat_map(state::Row::ids)
            .map(|id| id.0)
            .filter(|id| self.is_monitor(*id))
            .collect();
        self.sent_layout = Some(desktop);
        self.send(ClientMsg::Arrange { rows });
    }

    fn mirror_size(&self) -> Option<(ScreenId, u32, u32)> {
        let id = self.mirror_id()?;
        self.monitors
            .get(&id)
            .map(|m| (id, m.width_px, m.height_px))
    }

    /// Puts the laptop mirror back under the primary monitor, leaving the rest alone.
    fn place_mirror(&mut self) {
        let all = self.around_user(layout::arrange(
            self.settings.preset,
            &self.quest_sizes(),
            self.primary,
            self.mirror_size(),
            self.curve_factor(),
        ));
        if let Some(id) = self.mirror_id()
            && let Some(p) = all.get(&id)
        {
            self.placements.insert(id, *p);
        }
    }

    /// Moves the drawn monitors towards their places (held ones follow at once) and
    /// fades new ones in.
    fn animate(&mut self, dt: f32, held: &[ScreenId]) {
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
        // Where the monitors stand on the desk (in cinema, as saved before it).
        let desk = if self.settings.cinema {
            self.desk.load().placements
        } else {
            self.placements.clone()
        };
        let places: Option<Vec<Placement>> =
            sizes.iter().map(|m| desk.get(&m.0).copied()).collect();
        Profile {
            primary: sizes
                .iter()
                .position(|m| Some(m.0) == self.primary)
                .unwrap_or(0) as u8,
            sizes: sizes.iter().map(|m| (m.1, m.2)).collect(),
            mirror: self.mirror_wanted,
            settings: self.settings,
            places: places.unwrap_or_default(),
        }
    }

    /// Monitors without a place get one: all by the preset when none has any (the
    /// first start), else each next to the others (see `layout::place_new`), which
    /// stay where they are. The laptop screen goes under the primary monitor.
    fn place_missing(&mut self) {
        if self.settings.cinema || self.places_pending() {
            return;
        }
        let sizes = self.quest_sizes();
        if sizes.is_empty() {
            return;
        }
        if !sizes.iter().any(|m| self.placements.contains_key(&m.0)) {
            self.arrange_all();
            return;
        }
        let mut placed = false;
        for m in sizes {
            if self.placements.contains_key(&m.0) {
                continue;
            }
            let at = layout::place_new(
                &self.placements,
                &self.layout_sizes(),
                m,
                self.center,
                self.settings.preset,
                self.head.1,
                &self.sent_rows(),
                self.radius,
            );
            info!("monitor {} placed next to the others", m.0);
            self.placements.insert(m.0, at);
            placed = true;
        }
        if let Some(id) = self.mirror_id()
            && self.monitors.contains_key(&id)
            && !self.placements.contains_key(&id)
        {
            self.place_mirror();
            placed = true;
        }
        if placed {
            self.save_layout();
        }
    }

    /// A profile's places wait for the PC's new numbers.
    fn places_pending(&self) -> bool {
        self.pending_places
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() < *until)
    }

    /// Turns cinema mode on or off. The one place that switches it: the desk layout is
    /// saved before the cinema view replaces it (follow mode may have moved it), and
    /// comes back from disk after, window panels staying where they are.
    fn set_cinema(&mut self, on: bool) {
        if on == self.settings.cinema {
            return;
        }
        if on {
            self.save_layout();
            self.settings.cinema = true;
            self.arrange_all();
            return;
        }
        self.settings.cinema = false;
        let saved = self.desk.load();
        self.adopt_desk(saved);
        self.place_missing();
    }

    /// Cinema mode with no view yet (started in cinema, or switched to a PC in it):
    /// built once every monitor the PC has is here, so the chosen screen is among
    /// them. A view that exists, moved by hand perhaps, is left alone.
    fn build_cinema_view_if_due(&mut self) {
        let due = self.settings.cinema
            && !self.order.is_empty()
            && self.order.iter().all(|id| self.monitors.contains_key(id))
            && !self.order.iter().any(|id| self.placements.contains_key(id));
        if due {
            self.arrange_all();
        }
    }

    fn relayout_pending(&self) -> bool {
        self.relayout_until.is_some_and(|t| Instant::now() < t)
    }

    /// Saves the arrangement as the desk layout. In cinema mode `placements` is the
    /// cinema view, which is not saved: the desk layout stays as the user left it.
    /// Window panels are placed afresh when they appear, so they are not kept.
    fn save_layout(&self) {
        let desk: HashMap<ScreenId, Placement> = self
            .placements
            .iter()
            .filter(|(id, _)| !self.is_window(**id))
            .map(|(id, p)| (*id, *p))
            .collect();
        self.desk.save(
            &desk,
            &self.placed_for,
            (self.center, self.radius),
            self.settings.cinema,
        );
    }

    /// Takes `saved` as the desk arrangement. Window panels stay where they are; a
    /// monitor whose stream changed size since keeps the size of its pixels; layouts
    /// keep facing the way this one was laid out (from a 0.1.0 file, which does not
    /// say, the way its monitors face).
    fn adopt_desk(&mut self, saved: layout::Arrangement) {
        let windows: Vec<ScreenId> = self
            .placements
            .keys()
            .copied()
            .filter(|id| self.is_window(*id))
            .collect();
        self.placements.retain(|id, _| windows.contains(id));
        self.placements.extend(saved.placements);
        self.placed_for = saved.sizes;
        self.radius = saved.radius.unwrap_or(layout::RADIUS);
        let live: Vec<(ScreenId, layout::PixelSize)> = self
            .monitors
            .iter()
            .map(|(id, m)| (*id, (m.width_px, m.height_px)))
            .collect();
        for (id, size) in live {
            if !self.is_window(id) {
                self.fit_to_stream(id, size);
            }
        }
        self.center = saved.center.unwrap_or_else(|| {
            let desk: HashMap<ScreenId, Placement> = self
                .placements
                .iter()
                .filter(|(id, _)| !self.is_window(**id))
                .map(|(id, p)| (*id, *p))
                .collect();
            (
                Vec3::ZERO,
                layout::facing_yaw(&desk, Vec3::ZERO).unwrap_or(0.0),
            )
        });
    }

    /// The desk placement of monitor `id` follows its stream, now `size` pixels: the
    /// same size of a pixel as before (see `layout::refit`).
    fn fit_to_stream(&mut self, id: ScreenId, size: layout::PixelSize) {
        if let (Some(p), Some(&was)) = (self.placements.get_mut(&id), self.placed_for.get(&id)) {
            *p = layout::refit(*p, was, size);
        }
        self.placed_for.insert(id, size);
    }

    /// Lays everything out by the chosen preset.
    /// Layouts are worked out around the origin facing -Z; this moves them to where
    /// the user sat and looked when they last asked to bring everything in front.
    fn around_user(
        &self,
        mut placements: HashMap<ScreenId, Placement>,
    ) -> HashMap<ScreenId, Placement> {
        let (center, yaw) = self.center;
        let turn = glam::Quat::from_rotation_y(yaw);
        for p in placements.values_mut() {
            p.position = center + turn * p.position;
            p.rotation = turn * p.rotation;
        }
        placements
    }

    fn arrange_all(&mut self) {
        // Window panels stay where they are, whatever the layout.
        let panels: Vec<(ScreenId, Placement)> = self
            .placements
            .iter()
            .filter(|(id, _)| self.is_window(**id))
            .map(|(id, p)| (*id, *p))
            .collect();
        self.arrange_monitors();
        self.placements.extend(panels);
    }

    fn arrange_monitors(&mut self) {
        if self.settings.cinema {
            self.placements = self.around_user(layout::cinema(
                &self.quest_sizes(),
                self.settings.cinema_screen,
            ));
            self.grabs = [None, None];
            return;
        }
        let mirror = self.mirror_id();
        let mirror_place = mirror.and_then(|id| self.placements.get(&id).copied());
        self.radius = layout::preset_radius(self.settings.preset, &self.quest_sizes());
        self.placements = self.around_user(layout::arrange(
            self.settings.preset,
            &self.quest_sizes(),
            self.primary,
            self.mirror_size(),
            self.curve_factor(),
        ));
        // Keep the mirror's spot while the laptop lid is closed.
        if let (Some(id), Some(p)) = (mirror, mirror_place)
            && !self.placements.contains_key(&id)
        {
            self.placements.insert(id, p);
        }
        self.grabs = [None, None];
        self.save_layout();
        self.send_layout();
    }

    /// A new window panel: in front of the user, a little closer than the monitors and
    /// below eye level, beside panels already there. Flat, at the monitors' sharpness.
    fn place_window_panel(&mut self, id: ScreenId) {
        let Some(m) = self.monitors.get(&id) else {
            return;
        };
        const DISTANCE: f32 = 0.8;
        let (head, forward) = self.head;
        let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(-Vec3::Z);
        let right = Vec3::new(-flat.z, 0.0, flat.x);
        let others = self
            .placements
            .keys()
            .filter(|k| self.is_window(**k) && **k != id)
            .count();
        let width =
            (m.width_px as f32 * layout::METRES_PER_PIXEL * DISTANCE / self.radius).clamp(0.2, 1.2);
        let position = head + flat * DISTANCE - Vec3::Y * 0.12 + right * (0.15 * others as f32);
        self.placements.insert(
            id,
            Placement {
                position,
                rotation: layout::facing(position, Vec3::new(head.x, position.y, head.z)),
                width,
            },
        );
    }

    /// Monitor `id` as seen from where the user sits, for the menu's map: centre
    /// (right, up) and half size, in radians.
    /// Where the map looks from, and how to turn the room so it looks straight ahead:
    /// the eyes now, facing the arrangement; lying down, along the band tilted up
    /// overhead (seen from below, right over the head, left and right would make no
    /// sense), as the desktop does it.
    fn map_view(&self) -> (Vec3, glam::Quat) {
        let tilt = if self.settings.preset == crate::settings::Preset::Overhead {
            layout::OVERHEAD_PITCH
        } else {
            0.0
        };
        (
            self.view_eye.unwrap_or(self.center.0),
            glam::Quat::from_rotation_x(-tilt) * glam::Quat::from_rotation_y(-self.center.1),
        )
    }

    /// The monitors on the map: the desktop the PC gets (see `layout::Desktop`).
    fn map_spots(&self) -> HashMap<ScreenId, [f32; 4]> {
        self.desktop().spots()
    }

    /// The rows last sent to the PC, by id: what its desktop is laid out by now.
    fn sent_rows(&self) -> Vec<Vec<ScreenId>> {
        self.sent_layout
            .as_ref()
            .map(|d| d.rows.clone())
            .unwrap_or_default()
    }

    /// The PC desktop for the monitors and the laptop screen as they stand now, seen
    /// from where the arrangement faces.
    fn desktop(&self) -> layout::Desktop {
        self.desktop_memo.get(
            &self.placements,
            &self.layout_sizes(),
            self.center,
            self.settings.preset,
            self.primary,
            self.sent_layout.as_ref(),
        )
    }

    fn place_of(&self, id: ScreenId) -> ui::Place {
        let Some(p) = self.placements.get(&id) else {
            return ui::Place::Unknown;
        };
        let (eye, unturn) = self.map_view();
        let local = unturn * (p.position - eye);
        let side = local.x.atan2(-local.z);
        let up = local.y.atan2(Vec2::new(local.x, local.z).length());
        if up > 0.35 {
            ui::Place::Above
        } else if up < -0.35 {
            ui::Place::Below
        } else if side < -0.26 {
            ui::Place::Left
        } else if side > 0.26 {
            ui::Place::Right
        } else {
            ui::Place::Center
        }
    }

    /// Mouse follows the gaze: after resting on another monitor for a moment, the
    /// mouse goes to where the user looks on it.
    fn follow_gaze(&mut self) {
        let busy =
            self.wrist.is_some() || self.grabs.iter().any(Option::is_some) || self.settings.cinema;
        if !self.settings.gaze_mouse || !self.can_move_mouse || self.feedback.is_none() || busy {
            self.gaze_on = None;
            return;
        }
        let (head, forward) = self.head;
        let ray = Ray {
            origin: head,
            dir: forward.normalize_or(-Vec3::Z),
        };
        let curve = self.curve_radius();
        let looked = self
            .monitors
            .keys()
            .filter_map(|id| {
                let p = self.placements.get(id)?;
                let c = curve.filter(|_| !self.is_window(*id));
                let hit = layout::hit_surface(ray, p, self.height_of(*id), c)?;
                Some((*id, hit, *p, c))
            })
            .min_by(|a, b| a.1.t.total_cmp(&b.1.t));
        // Looking at the menu, not past it.
        let at_menu = self.panel_open
            && layout::hit_surface(ray, &self.panel_place, panel_height(), None)
                .is_some_and(|m| looked.as_ref().is_none_or(|l| m.t < l.1.t));
        let Some((id, hit, p, c)) = looked.filter(|_| !at_menu) else {
            self.gaze_on = None;
            return;
        };
        if self.focus == Some(id) {
            self.gaze_on = None;
            return;
        }
        match self.gaze_on {
            Some((on, since)) if on == id => {
                let recently = self
                    .gaze_moved
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
                if since.elapsed() >= GAZE_DWELL && !recently {
                    let uv = layout::image_uv(&p, self.height_of(id), c, hit.local);
                    let f = |v: f32| (v * 65535.0) as u16;
                    info!("gaze: mouse to monitor {id} at {:.2},{:.2}", uv.x, uv.y);
                    self.send(ClientMsg::MouseTo {
                        id: DisplayId(id),
                        u: f(uv.x),
                        v: f(uv.y),
                    });
                    self.focus = Some(id);
                    self.gaze_moved = Some(Instant::now());
                    self.gaze_on = None;
                }
            }
            _ => self.gaze_on = Some((id, Instant::now())),
        }
    }

    /// Rest reminders: after so many minutes of wearing, a short message. Every 20
    /// minutes it is the 20-20-20 rule (look 6 m away for 20 seconds); hourly, a break.
    /// Taking the headset off for 5 minutes counts as a rest.
    fn remind_rest(&mut self, dt: f32) {
        let every = u64::from(self.settings.rest_every) * 60;
        if every == 0 {
            return;
        }
        if !self.worn {
            self.off_since.get_or_insert_with(Instant::now);
            return;
        }
        if self
            .off_since
            .take()
            .is_some_and(|t| t.elapsed() >= Duration::from_secs(5 * 60))
        {
            self.worn_for = Duration::ZERO;
        }
        self.worn_for += Duration::from_secs_f32(dt);
        if self.worn_for.as_secs() < every {
            return;
        }
        self.worn_for = Duration::ZERO;
        let l = self.lang();
        let text = if every <= 20 * 60 {
            l.t(
                "Перепочиньте очі: 20 секунд подивіться вдалину",
                "Rest your eyes: look into the distance for 20 seconds",
            )
        } else {
            l.t(
                "Година в шоломі: час на коротку перерву",
                "An hour in the headset: time for a short break",
            )
        };
        self.show_toast(text.to_owned());
    }

    /// "Sound from the screen": the sound placed towards the monitor whose window plays
    /// it, as the head turns.
    fn place_sound(&self) {
        let angle = self
            .sound_from
            .filter(|_| self.settings.sound_from_screen)
            .and_then(|id| self.shown.get(&id))
            .map(|p| {
                // In the head's own terms, so lying down or tilting the head keeps the
                // ears right: how far towards the right ear, and how far in front.
                let (head, forward) = self.head;
                let to = (p.position - head).normalize_or_zero();
                // Right of where the user looks is positive.
                to.dot(self.head_right).atan2(to.dot(forward))
            });
        crate::audio::place(angle);
    }

    /// Shows `text` for a few seconds, below the line of sight.
    fn show_toast(&mut self, text: String) {
        // Not the text: it can be a PC notification (a message, a code).
        info!("toast shown");
        self.toast = Some((text, Instant::now(), None));
    }

    /// The monitor most straight ahead of the head (not a window panel).
    fn monitor_in_front(&self) -> Option<ScreenId> {
        let (head, forward) = self.head;
        self.placements
            .iter()
            .filter(|(id, _)| !self.is_window(**id) && self.monitors.contains_key(*id))
            .max_by(|a, b| {
                let score = |p: &Placement| (p.position - head).normalize_or_zero().dot(forward);
                score(a.1).total_cmp(&score(b.1))
            })
            .map(|(id, _)| *id)
    }

    /// Follow mode: once the head has turned (or moved) well away from where the
    /// monitors face, the whole arrangement glides after it until it is in front again.
    /// Small head movements leave it still, so reading is not disturbed.
    fn follow_head(&mut self, dt: f32) {
        const START_TURN: f32 = 0.35; // about 20°
        const START_MOVE: f32 = 0.25; // metres
        if !self.settings.follow
            || self.placements.is_empty()
            || self.grabs.iter().any(Option::is_some)
        {
            self.following = false;
            return;
        }
        let (head, forward) = self.head;
        let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(-Vec3::Z);
        let yaw = (-flat.x).atan2(-flat.z);
        let (pos, center_yaw) = self.center;
        let mut turn = yaw - center_yaw;
        turn =
            (turn + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI;
        let shift = Vec3::new(head.x - pos.x, 0.0, head.z - pos.z);
        if self.follow_hold {
            if turn.abs() < START_TURN && shift.length() < START_MOVE {
                self.follow_hold = false;
            }
            return;
        }
        if !self.following && (turn.abs() > START_TURN || shift.length() > START_MOVE) {
            self.following = true;
        }
        if !self.following {
            return;
        }
        // Ease towards the head: about a third of a second to catch up.
        let k = (dt * 3.0).min(1.0);
        let step_turn = turn * k;
        let step_shift = shift * k;
        let rot = glam::Quat::from_rotation_y(step_turn);
        let new_pos = pos + step_shift;
        for p in self.placements.values_mut() {
            p.position = new_pos + rot * (p.position - pos);
            p.rotation = rot * p.rotation;
        }
        self.center = (new_pos, center_yaw + step_turn);
        if turn.abs() < 0.02 && shift.length() < 0.02 {
            self.following = false;
            self.save_layout();
        }
    }

    /// Turns the whole arrangement around the user so the primary monitor is straight
    /// ahead, keeping how the monitors stand to each other.
    fn recenter(&mut self) {
        let (head, forward) = self.head;
        let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(-Vec3::Z);
        let yaw = (-flat.x).atan2(-flat.z);
        if self.settings.cinema || self.placements.is_empty() {
            self.center = (head, yaw);
            self.arrange_all();
            return;
        }
        let (old_pos, _) = self.center;
        // Straight ahead goes the primary monitor (else the middle of the monitors):
        // what the user arranged around it, not the layout's old forward, which hand
        // placement has long left behind.
        let direction = |p: &Placement| {
            let d = p.position - old_pos;
            (-d.x).atan2(-d.z)
        };
        let ahead = self
            .primary
            .and_then(|id| self.placements.get(&id))
            .map(direction)
            .or_else(|| {
                let (sin, cos) = self
                    .quest_sizes()
                    .iter()
                    .filter_map(|(id, _, _)| self.placements.get(id))
                    .map(direction)
                    .fold((0.0f32, 0.0f32), |(s, c), a| (s + a.sin(), c + a.cos()));
                (sin != 0.0 || cos != 0.0).then(|| sin.atan2(cos))
            })
            .unwrap_or(self.center.1);
        let turn = glam::Quat::from_rotation_y(yaw - ahead);
        for p in self.placements.values_mut() {
            p.position = head + turn * (p.position - old_pos);
            p.rotation = turn * p.rotation;
        }
        self.center = (head, yaw);
        self.save_layout();
        self.send_layout();
    }

    /// The A button: lines up what the user arranged by hand, row by row (see
    /// `layout::line_up`), instead of replacing it with the preset: every row keeps its
    /// place and the way it stands, tilted back or overhead too. Cinema rebuilds its
    /// own view.
    fn tidy_up(&mut self) {
        if self.settings.cinema || self.placements.is_empty() {
            self.arrange_all();
            return;
        }
        let lid = self
            .mirror_id()
            .filter(|id| self.placements.contains_key(id));
        let lined = layout::line_up(
            &self.placements,
            &self.layout_sizes(),
            self.center,
            self.settings.preset,
            self.primary,
            &self.sent_rows(),
            lid,
        );
        // Window panels (and anything not laid out) stay as they are.
        self.placements.extend(lined);
        self.grabs = [None, None];
        self.save_layout();
        self.send_layout();
    }

    /// The cylinder curved monitors are drawn on, times the arrangement's radius.
    fn curve_factor(&self) -> Option<f32> {
        self.curve_supported
            .then(|| self.settings.curve.radius_factor())
            .flatten()
    }

    fn curve_radius(&self) -> Option<f32> {
        self.curve_factor().map(|f| f * self.radius)
    }

    fn height_of(&self, id: ScreenId) -> f32 {
        match (self.placements.get(&id), self.monitors.get(&id)) {
            (Some(p), Some(m)) => p.width * m.aspect(),
            _ => 0.0,
        }
    }

    /// "Move all together": monitor `id` went from `before` to `after` in the user's
    /// hand, and the rest of the arrangement (monitors and the laptop screen, not
    /// window panels) goes along as one piece: moved, turned and scaled with it.
    fn carry_all(&mut self, id: ScreenId, before: &Placement, after: &Placement) {
        if !self.settings.move_all
            || self.settings.cinema
            || self.is_window(id)
            || !self.monitors.contains_key(&id)
        {
            return;
        }
        let turn = after.rotation * before.rotation.inverse();
        // One scale for all, kept so no monitor goes past the size limits: clamping
        // each on its own would pull the arrangement apart.
        let mut scale = after.width / before.width.max(1e-3);
        let windows: Vec<ScreenId> = self
            .placements
            .keys()
            .copied()
            .filter(|id| self.is_window(*id))
            .collect();
        for (other, p) in &self.placements {
            if *other != id && !windows.contains(other) {
                scale = scale.clamp(0.2 / p.width.max(1e-3), 8.0 / p.width.max(1e-3));
            }
        }
        for (other, p) in self.placements.iter_mut() {
            if *other == id || windows.contains(other) {
                continue;
            }
            p.position = after.position + turn * (p.position - before.position) * scale;
            p.rotation = (turn * p.rotation).normalize();
            p.width *= scale;
        }
    }

    /// What moves with the hands now: the held monitors, and with "move all together"
    /// every monitor that goes along (drawn where it is, not catching up, or the
    /// arrangement would tear apart while it moves).
    fn moving_now(&self, held: &[ScreenId]) -> Vec<ScreenId> {
        let carrying = self.settings.move_all
            && !self.settings.cinema
            && held
                .iter()
                .chain(self.scaling.as_ref().map(|s| &s.id))
                .any(|id| self.monitors.contains_key(id) && !self.is_window(*id));
        let mut ids = held.to_vec();
        if carrying {
            ids.extend(self.placements.keys().filter(|id| !self.is_window(**id)));
        }
        ids
    }

    /// The magnet: where monitor `id`, let go now, would stick (not on a wall, where
    /// monitors stay in its plane, nor for the laptop screen or window panels).
    fn stick_of(&self, id: ScreenId) -> Option<layout::Stick> {
        // Lying down the monitors are a tilted band the presets keep exact; window
        // panels float on their own.
        if self.settings.cinema
            || !self.settings.magnet
            || self.settings.move_all
            || self.settings.preset == crate::settings::Preset::Overhead
            || self.is_window(id)
            || !self.monitors.contains_key(&id)
        {
            return None;
        }
        if self.settings.preset == crate::settings::Preset::Wall {
            layout::stick_on_wall(&self.placements, &self.layout_sizes(), self.center, id)
        } else {
            layout::stick(&self.placements, &self.layout_sizes(), self.center.0, id)
        }
    }

    /// The monitors that make up the arrangement: the Quest monitors and the laptop
    /// screen, if it shows.
    fn layout_sizes(&self) -> Vec<(ScreenId, u32, u32)> {
        let mut sizes = self.quest_sizes();
        sizes.extend(self.mirror_size());
        sizes
    }

    /// Where the "Attach" button under detached window `id` is (as drawn, while it
    /// glides).
    fn attach_place(&self, id: ScreenId) -> Option<Placement> {
        let p = self.shown.get(&id).or_else(|| self.placements.get(&id))?;
        let down = p.width * self.monitors.get(&id)?.aspect() / 2.0 + ATTACH_GAP;
        Some(Placement {
            position: p.position + p.rotation * Vec3::new(0.0, -down, 0.005),
            rotation: p.rotation,
            width: ATTACH_WIDTH,
        })
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
        if let Some(w) = &self.wrist {
            consider(
                Target::Wrist,
                layout::hit_surface(ray, w, wrist_menu_height(), None),
            );
        }
        if let Some((id, _)) = self.attach_for
            && let Some(b) = self.attach_place(id)
        {
            consider(
                Target::Attach(id),
                layout::hit_surface(ray, &b, attach_height(), None),
            );
        }
        let curve = self.curve_radius();
        for id in self.monitors.keys() {
            if let Some(p) = self.placements.get(id) {
                consider(
                    Target::Monitor(*id),
                    layout::hit_surface(
                        ray,
                        p,
                        self.height_of(*id),
                        curve.filter(|_| !self.is_window(*id)),
                    ),
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
        // Hidden in the settings: the menu opens from the palm menu, the hand gesture
        // or the controller.
        if !self.settings.menu_button {
            self.button_place = None;
            return;
        }
        let shown = |id: &ScreenId| self.placements.contains_key(id);
        let anchor = self
            .primary
            .filter(shown)
            .or_else(|| self.quest_sizes().iter().map(|m| m.0).find(|id| shown(id)));
        let mirror = self
            .mirror_id()
            .filter(|id| self.monitors.contains_key(id))
            .and_then(|id| Some((*self.placements.get(&id)?, self.height_of(id))));
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
            width: panel_width(),
        };
        self.panel_open = true;
    }

    /// Tells the PC whether the headset is worn (quietly: nothing to show the user),
    /// and keeps it for the next `Open`.
    fn tell_presence(&self) {
        self.steering.set_headset(self.headset_state());
        if let Some(f) = &self.feedback {
            let _ = f.send(ClientMsg::Presence {
                part: Presence { worn: self.worn },
            });
        }
    }

    /// The headset's settings the PC acts on.
    fn prefs(&self) -> Prefs {
        prefs_of(&self.settings)
    }

    /// What the headset says about itself when it opens a session.
    fn headset_state(&self) -> HeadsetState {
        headset_state(&self.settings, self.worn, self.headset_battery)
    }

    /// The settings the PC acts on changed: it hears at once, and the next session
    /// starts with them.
    fn prefs_changed(&mut self) {
        self.steering.set_headset(self.headset_state());
        let part = self.prefs();
        if self.feedback.is_some() {
            self.send(ClientMsg::Prefs { part });
        }
    }

    /// Asks the PC to change something: one reply comes with its number. Without a
    /// connection nothing is sent (and nothing kept to send later: repeating "add a
    /// monitor" after reconnecting could add two).
    fn request(&mut self, asked: Request) -> Option<ReqId> {
        if self.feedback.is_none() {
            let what = self
                .lang()
                .t("Немає з'єднання з ПК", "No connection to the PC");
            self.show_error(what.into());
            return None;
        }
        self.last_req += 1;
        let req = ReqId(self.last_req);
        self.send(ClientMsg::request(req, asked));
        Some(req)
    }

    fn send(&mut self, msg: ClientMsg) {
        let sent = self.feedback.as_ref().map(|f| f.send(msg));
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
        let spots = self.map_spots();
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
            pc: self.pc.clone(),
            can_move_mouse: self.can_move_mouse,
            monitors: self
                .quest_sizes()
                .into_iter()
                .map(|(id, width, height)| MonitorEntry {
                    id,
                    width,
                    height,
                    place: self.place_of(id),
                    seen: spots.get(&id).copied(),
                })
                .collect(),
            windows: self
                .monitors
                .iter()
                .filter(|(id, _)| self.is_window(**id))
                .map(|(id, m)| MonitorEntry {
                    id: *id,
                    width: m.width_px,
                    height: m.height_px,
                    place: ui::Place::Unknown,
                    seen: None,
                })
                .collect(),
            primary: self.primary,
            selected: self.selected.filter(|id| self.monitors.contains_key(id)),
            mirror_seen: self
                .mirror_id()
                .filter(|id| self.monitors.contains_key(id))
                .and_then(|id| spots.get(&id).copied()),
            mirror_wanted: self.mirror_wanted,
            audio_wanted: self.audio_wanted,
            mic_wanted: self.mic_wanted,
            mic_denied: self.mic_wanted && crate::mic::denied(),
            mirror_active: self
                .mirror_id()
                .is_some_and(|id| self.monitors.contains_key(&id)),
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
            confirm_save: self
                .confirm_save
                .filter(|(_, at)| at.elapsed() < CONFIRM_FOR)
                .map(|(i, _)| i),
            paired: self.paired.iter().map(|(_, n)| n.clone()).collect(),
            battery: self.headset_battery,
            clock: local_time()
                .map(|t| clock_text(&t, self.settings.clock_12h))
                .unwrap_or_default(),
            date: local_time()
                .map(|t| date_text(&t, self.settings.date_style, self.lang()))
                .unwrap_or_default(),
            confirm_forget: self
                .confirm_forget
                .filter(|(_, at)| at.elapsed() < CONFIRM_FOR)
                .map(|(i, _)| i),
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
/// The menu tab as stored in the settings (the menu opens on the last one).
fn tab_code(tab: Tab) -> u8 {
    use ui::Section;
    match tab {
        Tab::Quick => 0,
        Tab::Monitors => 1,
        Tab::Settings(Section::Look) => 2,
        Tab::Settings(Section::Sound) => 3,
        Tab::Settings(Section::Language) => 4,
        Tab::Settings(Section::Pc) => 5,
        Tab::Settings(Section::Help) => 6,
        Tab::Settings(Section::Controls) => 7,
    }
}

fn tab_from_code(code: u8) -> Tab {
    use ui::Section;
    match code {
        1 => Tab::Monitors,
        2 => Tab::Settings(Section::Look),
        3 => Tab::Settings(Section::Sound),
        4 => Tab::Settings(Section::Language),
        5 => Tab::Settings(Section::Pc),
        6 => Tab::Settings(Section::Help),
        7 => Tab::Settings(Section::Controls),
        _ => Tab::Quick,
    }
}

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
        prostir_proto::app_version(env!("CARGO_PKG_VERSION")),
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
    set_menu_scale(settings.large_menu);
    crate::mic::set_allowed(settings.mic);

    // Passthrough keeps the cameras busy and costs battery: only when "Room" is chosen.
    let mut passthrough = if exts.fb_passthrough {
        passthrough::Passthrough::new(&session, settings.background.is_room())
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
    let mut toast_layer = ImageLayer::new(&session, format, TOAST_PX.0, TOAST_PX.1, false)?;
    let mut attach_layer = ImageLayer::new(&session, format, ATTACH_PX.0, ATTACH_PX.1, false)?;
    // What the "Attach" texture shows now: language and lit (pointed at).
    let mut attach_drawn: Option<(Lang, bool)> = None;
    cursor_layer.upload(&gl, &ui::cursor_image(CURSOR_PX))?;
    let mut button_hovered = false;
    // The left hand's menu gesture as last seen, to act once per gesture.
    let mut menu_gesture_held = false;
    // When the battery was last read, and whether the palm menu showed last frame.
    let mut last_battery: Option<Instant> = None;
    let mut palm_was_shown = false;
    button_layer.upload(&gl, &ui::menu_button_image(BUTTON_PX, false))?;
    let mut panel = Panel::new();
    let mut wrist_panel = Panel::wrist();
    let mut wrist_layer =
        ImageLayer::new(&session, format, ui::WRIST_WIDTH, ui::WRIST_HEIGHT, false)?;
    // The right fingertip left the wrist menu since its last press.
    let mut wrist_poke_armed = true;
    // Since when an open palm faces the face (its menu shows after a moment).
    let mut palm_since: Option<Instant> = None;
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
    // What the first `Open` says, before the network thread can connect.
    steering.set_headset(headset_state(&settings, true, crate::mic::battery()));
    // However this function ends, the network thread and its session end with it.
    struct StopNetwork(net::Steering);
    impl Drop for StopNetwork {
        fn drop(&mut self) {
            self.0.shut_down();
        }
    }
    let _stop_network = StopNetwork(steering.clone());
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

    let desk = layout::Desk::new(data_dir.join("layout.txt"));
    let mut scene = Scene {
        monitors: HashMap::new(),
        order: Vec::new(),
        // Filled from the desk below; in cinema mode the view is built once the PC's
        // monitors are here (see `build_cinema_view_if_due`) and the desk layout stays
        // on disk until cinema ends.
        placements: HashMap::new(),
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
            width: panel_width(),
        },
        button_place: None,
        grabs: [None, None],
        host: None,
        found: Vec::new(),
        ip_input: None,
        pairing: None,
        pair_offer: None,
        unreachable: None,
        pc: None,
        pc_displays: None,
        last_req: 0,
        steering,
        offline_since: Some(Instant::now()),
        feedback: None,
        error: None,
        confirm_remove: None,
        confirm_save: None,
        paired: Vec::new(),
        trust_file: data_dir.join("paired_hosts"),
        confirm_forget: None,
        tab: tab_from_code(settings.last_tab),
        update_needed: None,
        following: false,
        follow_hold: false,
        attach_for: None,
        wrist: None,
        selected: None,
        can_move_mouse: false,
        headset_battery: crate::mic::battery(),
        sound_from: None,
        refresh_due: false,
        worn_for: Duration::ZERO,
        off_since: None,
        gaze_on: None,
        gaze_moved: None,
        finder: None,
        toast: None,
        center: (Vec3::ZERO, 0.0),
        radius: layout::RADIUS,
        sent_layout: None,
        pending_layout: None,
        desktop_memo: layout::DesktopMemo::default(),
        head: (Vec3::ZERO, -Vec3::Z),
        head_right: Vec3::X,
        view_eye: None,
        hands_in_view: false,
        keyboard_place: layout::load(&data_dir.join("keyboard.txt")).remove(&0),
        keyboard_path: data_dir.join("keyboard.txt"),
        system_lang: crate::i18n::system(),
        profiles: profiles::load(&profiles_path),
        profiles_path,
        active_profile: None,
        relayout_until: None,
        pending_places: None,
        desk,
        placed_for: HashMap::new(),
        settings_path,
    };
    if !scene.settings.cinema {
        let saved = scene.desk.load();
        scene.adopt_desk(saved);
    }

    let mut hold_since: [Option<Instant>; 2] = [None, None];
    // A hand's pinch on a menu button: clicked when let go, unless it was held long
    // enough to drag the menu instead (as on the Quest's own windows).
    let mut pending_click: [Option<(ui::Action, Vec3)>; 2] = [None, None];
    // Grabs started on a monitor's bar (they end when the press ends).

    let mut pokes = [Poke::default(); 2];
    // Touches that just happened: (where, when), drawn as a ring spreading out.
    let mut rings: Vec<(Vec3, Instant)> = Vec::new();
    // Store builds: the store requires checking the user owns the app at launch.
    #[cfg(feature = "store")]
    let mut entitlement = crate::store::Entitlement::start();
    #[cfg(feature = "store")]
    let mut not_entitled_since: Option<Instant> = entitlement.is_none().then(Instant::now);
    let mut session_running = false;
    // The input is the app's: false while the system menu is open over it (the app
    // keeps drawing, but shows no hands or rays of its own and takes no input).
    let mut focused = false;
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
                    let now_focused = e.state() == xr::SessionState::FOCUSED;
                    if focused && !now_focused {
                        // A drag cannot go on under the system menu.
                        scene.grabs = [None, None];
                        scene.scaling = None;
                        scene.save_layout();
                    }
                    focused = now_focused;
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
                                set_refresh_rate(&session, scene.settings.battery_saver);
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
            let (made_for, job) = pano_job.take().expect("checked just above");
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
        if !focused {
            controls.state = Default::default();
        }
        let (head, forward, right) =
            view.locate(&local, time)
                .map_or((Vec3::ZERO, -Vec3::Z, Vec3::X), |l| {
                    let turn = xrutil::quat(l.pose.orientation);
                    (vec3(l.pose.position), turn * -Vec3::Z, turn * Vec3::X)
                });

        scene.head = (head, forward);
        scene.head_right = right;
        if scene.view_eye.is_none_or(|e| e.distance(head) > 0.05) && head != Vec3::ZERO {
            scene.view_eye = Some(head);
        }
        scene.follow_head(dt);
        scene.place_sound();
        scene.remind_rest(dt);
        if std::mem::take(&mut scene.refresh_due) && exts.fb_display_refresh_rate {
            set_refresh_rate(&session, scene.settings.battery_saver);
        }
        if focused {
            scene.follow_gaze();
        }

        // Tracked hands: joints for drawing, fingertips for touching the menu.
        let joints: [Option<[(Vec3, f32); 26]>; 2] = match &hand_trackers {
            Some(trackers) if focused && scene.settings.hands != HandMode::Hidden => {
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
        let menu_hand = usize::from(scene.settings.left_handed);
        scene.wrist = palm_menu_place(
            joints[menu_hand].as_ref(),
            menu_hand == 1,
            head,
            scene.wrist,
            &mut palm_since,
        );

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
            scene.tidy_up();
        }
        scene.place_button();
        // The headset's battery: on the palm menu (fresh when it appears, then every 15 s)
        // and for the PC's tray (once a minute).
        let palm_opened = scene.wrist.is_some() && !palm_was_shown;
        palm_was_shown = scene.wrist.is_some();
        let every = if scene.wrist.is_some() { 15 } else { 60 };
        if palm_opened
            || last_battery.is_none_or(|t: Instant| t.elapsed() >= Duration::from_secs(every))
        {
            last_battery = Some(Instant::now());
            scene.headset_battery = crate::mic::battery();
            scene.steering.set_headset(scene.headset_state());
            if let Some((percent, charging)) = scene.headset_battery
                && scene.feedback.is_some()
            {
                scene.send(ClientMsg::Battery {
                    part: Battery { percent, charging },
                });
            }
        }

        // Pointing, clicking and dragging.
        let mut picks: [Option<(Target, Hit)>; 2] =
            std::array::from_fn(|h| controls.state[h].ray.and_then(|r| scene.pick(r)));
        // A detached window pointed at shows "Attach" under it; it lingers so the
        // pointer can move down onto it.
        let pointed = picks.iter().flatten().find_map(|(t, _)| match t {
            Target::Monitor(id) | Target::Attach(id) if scene.is_window(*id) => Some(*id),
            _ => None,
        });
        if let Some(id) = pointed {
            scene.attach_for = Some((id, Instant::now()));
        } else if scene
            .attach_for
            .is_some_and(|(id, t)| t.elapsed() > ATTACH_LINGER || !scene.monitors.contains_key(&id))
        {
            scene.attach_for = None;
        }

        // The wrist menu: pointed at, or touched with the right index finger.
        let wrist_point = |local: Vec2| {
            (
                (local.x / WRIST_MENU_WIDTH + 0.5) * ui::WRIST_WIDTH as f32,
                (0.5 - local.y / wrist_menu_height()) * ui::WRIST_HEIGHT as f32,
            )
        };
        let mut wrist_hover = picks
            .iter()
            .flatten()
            .find_map(|(t, hit)| matches!(t, Target::Wrist).then(|| wrist_point(hit.local)));
        let mut wrist_pressed = false;
        let poking_hand = 1 - usize::from(scene.settings.left_handed);
        if let (Some(w), Some(tip)) = (scene.wrist, tips[poking_hand]) {
            let local = w.rotation.inverse() * (tip - w.position);
            let inside =
                local.x.abs() < WRIST_MENU_WIDTH / 2.0 && local.y.abs() < wrist_menu_height() / 2.0;
            if inside && local.z < 0.04 {
                wrist_hover = Some(wrist_point(Vec2::new(local.x, local.y)));
            }
            if inside && local.z < 0.008 && local.z > -0.03 && wrist_poke_armed {
                wrist_poke_armed = false;
                wrist_pressed = true;
                rings.push((tip, Instant::now()));
            }
            if !inside || local.z > 0.02 {
                wrist_poke_armed = true;
            }
        } else {
            wrist_poke_armed = true;
        }
        if scene.wrist.is_some() {
            let ui_state = scene.ui_state(
                passthrough.is_some(),
                passthrough.as_ref().is_some_and(|p| p.window_supported()),
                exts.khr_composition_layer_equirect2,
                custom_pano,
            );
            wrist_panel.set_state(&ui_state);
            wrist_panel.hover(wrist_hover);
            if wrist_pressed && let Some(action) = wrist_panel.hovered_action() {
                controls.pulse(&session, poking_hand);
                handle_action(&mut scene, action, &mut passthrough, &mut controls);
            }
        }
        let panel_point = |hit: &Hit| {
            let x = (hit.local.x / panel_width() + 0.5) * ui::WIDTH as f32;
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
            let (half_w, half_h) = (panel_width() / 2.0, panel_height() / 2.0);
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
                (l.x / panel_width() + 0.5) * ui::WIDTH as f32,
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
                rings.push((
                    place.position + place.rotation * Vec3::new(l.x, l.y, 0.003),
                    Instant::now(),
                ));
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
                (at.x / panel_width() + 0.5) * ui::WIDTH as f32,
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
            let before = *p;
            let d = a.origin.distance(b.origin);
            p.width = (s.start_width * d / s.start_distance.max(0.05)).clamp(0.2, 8.0);
            let after = *p;
            scene.carry_all(s.id, &before, &after);
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
                    Some((Target::Attach(id), hit)) => {
                        clicked = true;
                        rings.push((hit.point, Instant::now()));
                        controls.pulse(&session, hand);
                        scene.attach_for = None;
                        handle_action(
                            &mut scene,
                            Action::CloseWindow(id),
                            &mut passthrough,
                            &mut controls,
                        );
                    }
                    Some((Target::Panel, hit)) => {
                        panel.hover(Some(panel_point(&hit)));
                        if let Some(action) = panel.hovered_action() {
                            if input.is_hand {
                                // Decided when the pinch opens: a click, or a drag.
                                pending_click[hand] = Some((action, hit.point));
                            } else {
                                rings.push((hit.point, Instant::now()));
                                clicked = true;
                                controls.pulse(&session, hand);
                                handle_action(&mut scene, action, &mut passthrough, &mut controls);
                            }
                        }
                    }
                    Some((Target::Wrist, hit)) => {
                        clicked = true;
                        wrist_panel.hover(Some(wrist_point(hit.local)));
                        if let Some(action) = wrist_panel.hovered_action() {
                            rings.push((hit.point, Instant::now()));
                            controls.pulse(&session, hand);
                            handle_action(&mut scene, action, &mut passthrough, &mut controls);
                        }
                    }
                    Some((Target::MenuButton, hit)) => {
                        rings.push((hit.point, Instant::now()));
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
                if let Some((action, point)) = pending_click[hand].take() {
                    rings.push((point, Instant::now()));
                    controls.pulse(&session, hand);
                    handle_action(&mut scene, action, &mut passthrough, &mut controls);
                }
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
                scene.save_layout();
            }
            if end_grab && let Some(released) = scene.grabs[hand].take() {
                // A monitor put somewhere new stays there until the head comes back
                // near where it was (the menu and the keyboard do not count).
                scene.follow_hold = scene.settings.follow
                    && released
                        .id
                        .screen()
                        .is_some_and(|id| scene.placements.contains_key(&id));
                // Letting go with the first hand ends resizing too.
                scene.scaling = None;
                match released.id {
                    Held::Keyboard => {
                        if let Some(k) = scene.keyboard_place {
                            layout::save(&scene.keyboard_path, &HashMap::from([(0, k)]));
                        }
                    }
                    Held::Menu if !scene.settings.cinema => scene.save_layout(),
                    Held::Screen(id) if !scene.settings.cinema => {
                        // Let go next to another monitor: it sticks to it, edge to edge.
                        if let Some(s) = scene.stick_of(id) {
                            scene.placements.insert(id, s.placement);
                            controls.pulse(&session, hand);
                        }
                        scene.save_layout();
                        // Dropped somewhere else: KDE's arrangement follows what the
                        // eyes see.
                        scene.send_layout();
                    }
                    Held::Menu | Held::Screen(_) => {}
                }
            }
            let Some(ray) = input.ray else { continue };
            if let Some(mut grab) = scene.grabs[hand] {
                let target = match grab.id {
                    Held::Menu => Some(&mut scene.panel_place),
                    Held::Keyboard => scene.keyboard_place.as_mut(),
                    Held::Screen(id) => scene.placements.get_mut(&id),
                };
                let wall = scene.settings.preset == crate::settings::Preset::Wall
                    && !scene.settings.cinema;
                let wall_facing = glam::Quat::from_rotation_y(scene.center.1);
                if let Some(p) = target {
                    let before = *p;
                    grab.update(p, ray, head, input.stick, dt);
                    if grab.id == Held::Keyboard {
                        // It follows the hand but stays flat, like the desk.
                        p.rotation = lying_flat(p.position, head);
                    } else if wall && grab.id != Held::Menu {
                        // On a wall, monitors stay parallel to it wherever they go.
                        p.rotation = wall_facing;
                    }
                    let after = *p;
                    scene.grabs[hand] = Some(grab);
                    if let Some(id) = grab.id.screen() {
                        scene.carry_all(id, &before, &after);
                    }
                }
            } else if start_grab {
                hold_since[hand] = None;
                // Held on a button: the menu moves, the button is not pressed.
                pending_click[hand] = None;
                let other = scene.grabs[1 - hand].map(|g| g.id);
                let grabbed = match picks[hand] {
                    Some((Target::Panel, hit)) if other != Some(Held::Menu) => Some(Grab::start(
                        Held::Menu,
                        hit.t,
                        hit.local,
                        &scene.panel_place,
                    )),
                    Some((Target::Keyboard, hit)) if other != Some(Held::Keyboard) => scene
                        .keyboard_place
                        .map(|k| Grab::start(Held::Keyboard, hit.t, hit.local, &k)),
                    // The other hand already holds it: resize with both.
                    Some((Target::Monitor(id), _)) if other == Some(Held::Screen(id)) => {
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
                    Some((Target::Monitor(id), hit)) if other != Some(Held::Screen(id)) => Some(
                        Grab::start(Held::Screen(id), hit.t, hit.local, &scene.placements[&id]),
                    ),
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
            // And paired with another PC.
            scene.paired = prostir_proto::secure::TrustStore::load(&scene.trust_file)
                .entries()
                .to_vec();
        }
        crate::i18n::set_current(scene.lang());
        panel_was_open = scene.panel_open;
        // A new toast: drawn once, placed where the user looks now.
        if let Some((text, _, place @ None)) = &mut scene.toast {
            toast_layer.upload(&gl, &panel.toast_image(text, TOAST_PX.0, TOAST_PX.1))?;
            let (head, forward) = scene.head;
            let at = head + forward.normalize_or(-Vec3::Z) * 0.7 - Vec3::Y * 0.18;
            *place = Some(Placement {
                position: at,
                rotation: layout::facing(at, head),
                width: TOAST_WIDTH,
            });
        }
        if scene
            .toast
            .as_ref()
            .is_some_and(|t| t.1.elapsed() > TOAST_FOR)
        {
            scene.toast = None;
        }
        if scene.attach_for.is_some() {
            let lit = picks
                .iter()
                .flatten()
                .any(|(t, _)| matches!(t, Target::Attach(_)));
            let want = (scene.lang(), lit);
            if attach_drawn != Some(want) {
                let label = scene.lang().t("Прикріпити", "Attach");
                attach_layer
                    .upload(&gl, &panel.pill_image(label, ATTACH_PX.0, ATTACH_PX.1, lit))?;
                attach_drawn = Some(want);
            }
        }
        if scene.wrist.is_some()
            && let Some(pixels) = wrist_panel.render_if_dirty()
        {
            wrist_layer.upload(&gl, pixels)?;
        }
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
        // Light on the menu where a fingertip is about to touch: wide and faint from a
        // few centimetres away, small and bright right before contact.
        let place = scene.panel_place;
        for (h, hover) in poke_hover.iter().enumerate() {
            let Some((at, z)) = hover else { continue };
            let near = (1.0 - z.max(0.0) / POKE_HOVER).clamp(0.0, 1.0);
            let pressed = if pokes[h].pressed { 0.25 } else { 0.0 };
            spheres.push(Sphere::glow(
                place.position + place.rotation * Vec3::new(at.x, at.y, 0.003),
                0.034 - 0.020 * near,
                [
                    0.72,
                    0.84,
                    1.0,
                    (0.12 + 0.6 * near * near + pressed).min(1.0),
                ],
            ));
        }
        // A touch spreads out as a ring for a moment.
        rings.retain(|(_, at)| at.elapsed() < TOUCH_RING);
        for (center, at) in &rings {
            let k = at.elapsed().as_secs_f32() / TOUCH_RING.as_secs_f32();
            spheres.push(Sphere::ring(
                *center,
                0.012 + 0.03 * k,
                [0.8, 0.9, 1.0, 0.9 * (1.0 - k)],
            ));
        }
        // Where a ray points: a small light on the menu, a wider faint one on monitors.
        for pick in picks.iter().flatten() {
            let (radius, alpha) = match pick.0 {
                Target::Panel | Target::MenuButton | Target::Keyboard => (0.012, 0.55),
                Target::Monitor(_) => (0.03, 0.3),
                Target::Attach(_) => (0.01, 0.5),
                Target::Wrist => (0.008, 0.55),
            };
            spheres.push(Sphere::glow(pick.1.point, radius, [0.72, 0.84, 1.0, alpha]));
        }
        // Where the mouse is: rings closing in on it, three times.
        if let Some((id, uv, at)) = scene.finder {
            let age = at.elapsed();
            match (scene.shown.get(&id), age < FINDER_FOR) {
                (Some(p), true) => {
                    let c = scene.curve_radius().filter(|_| !scene.is_window(id));
                    let (point, _) = layout::image_point(p, scene.height_of(id), c, uv);
                    let t = age.as_secs_f32() / FINDER_FOR.as_secs_f32();
                    for k in 0..3 {
                        let phase = (t * 3.0 - k as f32 * 0.33).rem_euclid(1.0);
                        spheres.push(Sphere::ring(
                            point,
                            0.09 * (1.0 - phase) + 0.01,
                            [1.0, 0.85, 0.35, 0.9 * phase * (1.0 - t * 0.5)],
                        ));
                    }
                    spheres.push(Sphere::glow(point, 0.02, [1.0, 0.85, 0.35, 0.8]));
                }
                _ => scene.finder = None,
            }
        }
        // The magnet while dragging: the edge the monitor would stick to lights up.
        for g in scene.grabs.iter().flatten() {
            let Some(s) = g.id.screen().and_then(|id| scene.stick_of(id)) else {
                continue;
            };
            let Some(n) = scene.shown.get(&s.neighbour).copied() else {
                continue;
            };
            let (half_w, half_h) = (n.width / 2.0, scene.height_of(s.neighbour) / 2.0);
            let steps = 28;
            for i in 0..=steps {
                let k = i as f32 / steps as f32 * 2.0 - 1.0;
                let local = if s.across {
                    Vec3::new(s.side * half_w, k * half_h, 0.01)
                } else {
                    Vec3::new(k * half_w, s.side * half_h, 0.01)
                };
                spheres.push(Sphere::glow(
                    n.position + n.rotation * local,
                    0.03,
                    [0.55, 0.75, 1.0, 0.55],
                ));
            }
        }
        // Hands are drawn over everything (the compositor has no depth for them), so
        // near a menu they would cover its text: the hand holding the palm menu is not
        // drawn, and a hand about to touch a menu is see-through.
        for (h, j) in joints.iter().enumerate() {
            let Some(j) = j else { continue };
            if h == menu_hand && scene.wrist.is_some() {
                continue;
            }
            let tip = j[INDEX_TIP].0;
            let near_palm = scene.wrist.is_some_and(|w| w.position.distance(tip) < 0.15);
            let near_panel = poke_hover[h].is_some();
            let alpha = if near_palm || near_panel { 0.3 } else { 1.0 };
            overlay::hand_spheres(j, alpha, &mut spheres);
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
                        kind: overlay::Kind::Ball,
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
        let grabbed: Vec<ScreenId> = scene
            .grabs
            .iter()
            .flatten()
            .filter_map(|g| g.id.screen())
            .collect();
        let moving = scene.moving_now(&grabbed);
        scene.animate(dt, &moving);
        // Keep KDE's arrangement in step once nothing is being moved.
        if grabbed.is_empty() && scene.scaling.is_none() && !scene.relayout_pending() {
            scene.send_layout();
        }
        let mut order: Vec<(f32, ScreenId)> = scene
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
                // Focus dimming, the brightness setting and fading in, together.
                let b = scene.brightness.get(id).copied().unwrap_or(1.0)
                    * t
                    * f32::from(scene.settings.brightness)
                    / 100.0;
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
            // Window panels stay flat.
            if let Some(radius) = curve.filter(|_| !scene.is_window(*id)) {
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
                panel_width(),
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
        let toast_quad = scene.toast.as_ref().and_then(|t| t.2).map(|p| {
            toast_layer.quad(
                &local,
                posef(&p),
                TOAST_WIDTH,
                TOAST_WIDTH * TOAST_PX.1 as f32 / TOAST_PX.0 as f32,
            )
        });
        layers.extend(toast_quad.iter().map(as_layer));
        let attach_quad = scene
            .attach_for
            .filter(|_| attach_drawn.is_some())
            .and_then(|(id, _)| scene.attach_place(id))
            .map(|p| attach_layer.quad(&local, posef(&p), ATTACH_WIDTH, attach_height()));
        layers.extend(attach_quad.iter().map(as_layer));
        let wrist_quad = scene
            .wrist
            .filter(|_| wrist_layer.ready())
            .map(|w| wrist_layer.quad(&local, posef(&w), WRIST_MENU_WIDTH, wrist_menu_height()));
        layers.extend(wrist_quad.iter().map(as_layer));
        layers.extend(hands_layer.iter().map(as_layer));
        layers.extend(cursors.iter().map(as_layer));
        // The compositor supports 16 layers at most.
        layers.truncate(16);
        match frame_stream.end(time, xr::EnvironmentBlendMode::OPAQUE, &layers) {
            // One frame the compositor refused is not worth closing the app over.
            Err(
                e @ (xr::sys::Result::ERROR_POSE_INVALID
                | xr::sys::Result::ERROR_LAYER_INVALID
                | xr::sys::Result::ERROR_SWAPCHAIN_RECT_INVALID),
            ) => {
                warn!("frame refused: {e}");
            }
            other => other?,
        }
    }

    info!("exiting");
    drop(scene);
    Ok(())
}

/// What to tell the user about a connection that ended; nothing where there is
/// nothing to say (the headset was off the head, or the PC already said why).
fn disconnect_text(l: Lang, why: net::Disconnect) -> Option<&'static str> {
    use prostir_proto::PcGoodbye;
    Some(match why {
        net::Disconnect::Goodbye(PcGoodbye::Quit) => {
            l.t("Програму на ПК закрито", "The PC app was closed")
        }
        net::Disconnect::Goodbye(PcGoodbye::Forgotten) => l.t(
            "ПК більше не знає цей шолом. Сполучіть знову",
            "The PC no longer knows this headset. Pair them again",
        ),
        net::Disconnect::Goodbye(PcGoodbye::Other) => {
            l.t("ПК завершив з'єднання", "The PC ended the connection")
        }
        net::Disconnect::Silence => l.t("Немає зв'язку з ПК", "No connection to the PC"),
        net::Disconnect::Goodbye(PcGoodbye::TakenOff | PcGoodbye::Replaced | PcGoodbye::Failed)
        | net::Disconnect::Closed => return None,
    })
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
            opened_with,
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
            scene.last_req = 0;
            scene.sent_layout = None;
            scene.update_needed = None;
            // The PC follows the headset's microphone switch.
            scene.send(ClientMsg::SetMic {
                enabled: scene.settings.mic,
            });
            // What changed since `Open` went (it carried the state of that moment).
            let now = scene.headset_state();
            if now.prefs != opened_with.prefs {
                scene.prefs_changed();
            }
            if now.presence != opened_with.presence {
                scene.tell_presence();
            }
            scene.offline_since = None;
            scene.ip_input = None;
            scene.unreachable = None;
        }
        NetEvent::Hosts(hosts) => scene.found = hosts,
        NetEvent::Unreachable(name) => scene.unreachable = name,
        NetEvent::Encoding(encoding) => scene.pc = Some(encoding),
        NetEvent::Sound(sound) => scene.audio_wanted = sound.to_headset,
        NetEvent::HostCaps(pc) => {
            info!("the PC can: {:?}", pc.features);
            scene.can_move_mouse = pc.has(caps::pc::MOVE_MOUSE);
        }
        NetEvent::Recenter => scene.recenter(),
        NetEvent::Notice(text) => scene.show_toast(text),
        NetEvent::Reply { req, outcome } => match outcome {
            Outcome::Failed { detail, .. } => {
                warn!("request {} failed", req.0);
                scene.show_error(detail);
            }
            Outcome::Saved { path } => {
                let saved = scene.lang().t("Знімок збережено:", "Screenshot saved:");
                scene.show_toast(format!("{saved} {path}"));
            }
            Outcome::Pointer { id, u, v } => {
                let f = |v: u16| f32::from(v) / 65535.0;
                scene.finder = Some((id.0, Vec2::new(f(u), f(v)), Instant::now()));
            }
            Outcome::Done | Outcome::Added { .. } | Outcome::Ids { .. } => {}
        },
        NetEvent::Problem { error, detail } => {
            warn!("a problem on the PC (code {})", error.code());
            scene.show_error(detail);
        }
        NetEvent::SoundFrom(id) => scene.sound_from = id.map(|id| id.0),
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
        NetEvent::PairOffer { host, why } => {
            scene.pairing = None;
            scene.pair_offer = Some((host, why));
        }
        NetEvent::Incompatible { pc_newer, pc_app } => {
            warn!("the PC runs Prostir {pc_app}: versions differ");
            scene.update_needed = Some((pc_newer, pc_app));
        }
        NetEvent::Disconnected { why } => {
            scene.offline_since = Some(Instant::now());
            scene.pairing = None;
            scene.host = None;
            scene.pc = None;
            scene.feedback = None;
            // The next session's list is news, not displays removed.
            scene.order.clear();
            scene.pc_displays = None;
            if let Some(text) = disconnect_text(scene.lang(), why) {
                scene.show_toast(text.into());
            }
        }
        NetEvent::Focus(id) => scene.focus = id.map(|id| id.0),
        NetEvent::Displays(displays) => displays_changed(scene, displays),
        NetEvent::HostError(text) => scene.show_error(text),
        NetEvent::StreamStart {
            id,
            generation,
            width,
            height,
            codec,
            frames,
            feedback,
        } => {
            let stream = decoder::Stream { id, generation };
            let id = id.0;
            scene.monitors.remove(&id);
            match xrutil::create_surface_swapchain(session, vm, width, height) {
                Ok((swapchain, window)) => {
                    let stop = Arc::new(AtomicBool::new(false));
                    let decoder = decoder::spawn(
                        stream,
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
                    stream_shown(scene, id);
                }
                Err(e) => error!("monitor {id}: {e:#}"),
            }
        }
        // The stream stopped; the display keeps its place (only the list says it is gone).
        NetEvent::StreamEnd { id } => {
            scene.monitors.remove(&id.0);
        }
    }
}

/// A stream of display `id` shows: it gets its place (a display not listed yet waits
/// for the list, unseen).
fn stream_shown(scene: &mut Scene, id: ScreenId) {
    let Some(kind) = scene.kind(id).cloned() else {
        return;
    };
    let size = scene.monitors.get(&id).map(|m| (m.width_px, m.height_px));
    if let (DisplayKind::Virtual, Some(size)) = (&kind, size)
        && !scene.settings.cinema
    {
        scene.fit_to_stream(id, size);
    }
    match kind {
        DisplayKind::Window { .. } => {
            if !scene.placements.contains_key(&id) {
                scene.place_window_panel(id);
            }
        }
        _ if scene.relayout_pending() => scene.arrange_all(),
        // A monitor the arrangement has no place for: next to the others, which
        // stay. One it knows stays where it is.
        _ if !scene.placements.contains_key(&id) => scene.place_missing(),
        _ => {}
    }
    scene.build_cinema_view_if_due();
}

/// The PC listed its displays anew: the one source of which there are. Within a
/// session, a display that left the list is gone, and its place goes with it (a
/// display of the PC itself keeps its place: it comes back where it was).
fn displays_changed(scene: &mut Scene, displays: state::Displays) {
    let ids: Vec<ScreenId> = displays.ids().map(|id| id.0).collect();
    let gone: Vec<ScreenId> = scene
        .pc_displays
        .iter()
        .flat_map(|d| &d.displays)
        .filter(|d| !ids.contains(&d.id.0) && !d.is_physical())
        .map(|d| d.id.0)
        .collect();
    let order: Vec<ScreenId> = displays
        .displays
        .iter()
        .filter(|d| d.is_virtual())
        .map(|d| d.id.0)
        .collect();
    let primary = displays.primary.map(|p| p.0);
    scene.mirror_wanted = displays.displays.iter().any(|d| d.is_shown_physical());
    scene.pc_displays = Some(displays);
    for id in &gone {
        scene.placements.remove(id);
        scene.monitors.remove(id);
        for g in &mut scene.grabs {
            if g.is_some_and(|g| g.id == Held::Screen(*id)) {
                *g = None;
            }
        }
    }
    if primary != scene.primary {
        info!("primary display: {primary:?}");
        scene.primary = primary;
        if scene.relayout_pending() {
            scene.arrange_all();
        }
    }
    scene.order = order;
    // A profile's places on the numbers its monitors have now (the last list of the
    // right length while the PC makes them counts).
    if scene.places_pending()
        && let Some((places, _)) = &scene.pending_places
        && places.len() == scene.order.len()
        && !scene.settings.cinema
    {
        for (id, p) in scene.order.clone().into_iter().zip(places.clone()) {
            scene.placements.insert(id, p);
            scene.placed_for.remove(&id);
        }
        scene.save_layout();
    } else if scene.relayout_pending() {
        scene.arrange_all();
    } else if !gone.is_empty() {
        scene.save_layout();
    }
    // Streams that came before the list said what they are.
    let waiting: Vec<ScreenId> = scene
        .monitors
        .keys()
        .copied()
        .filter(|id| !scene.placements.contains_key(id))
        .collect();
    for id in waiting {
        stream_shown(scene, id);
    }
    scene.place_missing();
    scene.build_cinema_view_if_due();
}

/// The headset's settings the PC acts on, from `s`.
fn prefs_of(s: &Settings) -> Prefs {
    Prefs {
        notifications: s.notifications,
        sound_from_screen: s.sound_from_screen,
        focus_mode: s.focus,
        gaze_mouse_ms: s
            .gaze_mouse
            .then(|| u16::try_from(GAZE_DWELL.as_millis()).unwrap_or(u16::MAX)),
        no_limits: false,
    }
}

/// What the headset says about itself when it opens a session: what it can do, its
/// settings, whether it is worn, its battery.
fn headset_state(settings: &Settings, worn: bool, battery: Option<(u8, bool)>) -> HeadsetState {
    HeadsetState {
        caps: Some(headset_caps()),
        prefs: Some(prefs_of(settings)),
        presence: Some(Presence { worn }),
        battery: battery.map(|(percent, charging)| Battery { percent, charging }),
    }
}

/// What this headset can do, for the PC: the displays it shows, the codecs it decodes
/// and how large (Quest 3: H.264 up to 4K, HEVC up to 8K), how many streams and
/// compositor layers at once.
fn headset_caps() -> HeadsetCaps {
    let decoder = |codec: Codec, max_w, max_h| state::Decoder {
        codec: prostir_proto::wire::encode(&codec).map_or(0, |b| b[0]),
        max_w,
        max_h,
    };
    HeadsetCaps {
        features: vec![caps::headset::PHYSICAL, caps::headset::WINDOWS],
        decoders: vec![
            decoder(Codec::H264, 4096, 4096),
            decoder(Codec::H265, 8192, 4320),
        ],
        streams_max: 16,
        layers_max: 16,
        model: "Meta Quest".into(),
    }
}

/// Passthrough cameras on only for the "room" background.
fn set_passthrough(scene: &mut Scene, passthrough: &mut Option<passthrough::Passthrough>) {
    let background = scene.settings.background;
    let opacity = if background == Background::RoomDim {
        0.4
    } else {
        1.0
    };
    if let Some(pt) = passthrough
        && let Err(e) = pt
            .set_room(background.is_room())
            .and_then(|()| pt.set_room_opacity(opacity))
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
        Action::Add { width, height } => {
            scene.request(Request::AddDisplay {
                mode: Mode::new(width, height),
            });
        }
        Action::Remove(id) => {
            let confirmed = scene
                .confirm_remove
                .is_some_and(|(c, at)| c == id && at.elapsed() < CONFIRM_FOR);
            if confirmed {
                scene.confirm_remove = None;
                scene.request(Request::RemoveDisplay { id: DisplayId(id) });
            } else {
                scene.confirm_remove = Some((id, Instant::now()));
            }
        }
        Action::Tab(tab) => {
            scene.tab = tab;
            scene.settings.last_tab = tab_code(tab);
            scene.settings.save(settings_path);
        }
        Action::Recenter => {
            scene.recenter();
            // An open menu comes along, in front again; a closed one stays closed (the
            // palm menu has this button too).
            if scene.panel_open {
                let (head, forward) = scene.head;
                let touch = scene.hands_in_view;
                scene.open_panel(head, forward, touch);
            }
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
            scene.set_cinema(on);
            scene.settings.save(settings_path);
        }
        Action::KeyboardWindow(on) => {
            scene.settings.keyboard_window = on;
            scene.settings.save(settings_path);
        }
        Action::Focus(on) => {
            scene.settings.focus = on;
            scene.settings.save(settings_path);
            scene.prefs_changed();
        }
        Action::DetachWindow => {
            scene.request(Request::DetachWindow);
            // The panel appears in front: the menu steps aside.
            scene.panel_open = false;
        }
        Action::WindowHere => {
            if let Some(id) = scene.monitor_in_front() {
                scene.request(Request::WindowHere { id: DisplayId(id) });
            }
        }
        Action::Screenshot => {
            if let Some(id) = scene.monitor_in_front() {
                scene.request(Request::Screenshot { id: DisplayId(id) });
            }
        }
        Action::CloseWindow(id) => {
            scene.request(Request::RemoveDisplay { id: DisplayId(id) });
        }
        Action::Tidy => scene.tidy_up(),
        Action::SelectMonitor(id) => {
            scene.selected = Some(id);
            scene.confirm_remove = None;
        }
        Action::FindMouse => {
            scene.request(Request::FindMouse);
        }
        Action::GazeMouse(on) => {
            scene.settings.gaze_mouse = on;
            scene.settings.save(settings_path);
            scene.prefs_changed();
        }
        Action::LargeMenu(on) => {
            scene.settings.large_menu = on;
            scene.settings.save(settings_path);
            set_menu_scale(on);
            scene.panel_place.width = panel_width();
        }
        Action::RestEvery(minutes) => {
            scene.settings.rest_every = minutes;
            scene.settings.save(settings_path);
            scene.worn_for = Duration::ZERO;
        }
        Action::Brightness(percent) => {
            scene.settings.brightness = percent;
            scene.settings.save(settings_path);
        }
        Action::Clock12h(on) => {
            scene.settings.clock_12h = on;
            scene.settings.save(settings_path);
        }
        Action::DateStyle(style) => {
            scene.settings.date_style = style;
            scene.settings.save(settings_path);
        }
        Action::MenuButton(on) => {
            scene.settings.menu_button = on;
            scene.settings.save(settings_path);
        }
        Action::SoundFromScreen(on) => {
            scene.settings.sound_from_screen = on;
            scene.settings.save(settings_path);
            scene.prefs_changed();
            if !on {
                scene.sound_from = None;
            }
        }
        Action::Notifications(on) => {
            scene.settings.notifications = on;
            scene.settings.save(settings_path);
            scene.prefs_changed();
        }
        Action::BatterySaver(on) => {
            scene.settings.battery_saver = on;
            scene.settings.save(settings_path);
            scene.refresh_due = true;
        }
        Action::LeftHanded(on) => {
            scene.settings.left_handed = on;
            scene.settings.save(settings_path);
        }
        Action::OpenMenu => {
            let (head, forward) = scene.head;
            let touch = scene.hands_in_view;
            scene.open_panel(head, forward, touch);
        }
        Action::MoveAll(on) => {
            scene.settings.move_all = on;
            scene.settings.save(settings_path);
        }
        Action::Magnet(on) => {
            scene.settings.magnet = on;
            scene.settings.save(settings_path);
        }
        Action::Follow(on) => {
            scene.settings.follow = on;
            scene.follow_hold = false;
            scene.settings.save(settings_path);
            if !on {
                scene.save_layout();
            }
        }
        Action::Hands(mode) => {
            scene.settings.hands = mode;
            scene.settings.save(settings_path);
            controls.hands_enabled = mode == HandMode::Control;
        }
        Action::MakePrimary(id) => {
            scene.request(Request::SetPrimary { id: DisplayId(id) });
        }
        // Swaps the places of a monitor and its neighbour in the order; the others
        // stay.
        Action::Move { id, delta } => {
            let ids: Vec<ScreenId> = scene.quest_sizes().iter().map(|m| m.0).collect();
            if let Some(i) = ids.iter().position(|x| *x == id) {
                let j = (i as i32 + delta).clamp(0, ids.len() as i32 - 1) as usize;
                let other = ids[j];
                if let (Some(a), Some(b)) = (
                    scene.placements.get(&id).copied(),
                    scene.placements.get(&other).copied(),
                ) {
                    // Each at the other's place, at the size of its own pixels.
                    let size =
                        |id: ScreenId| scene.monitors.get(&id).map(|m| (m.width_px, m.height_px));
                    if let (Some(sa), Some(sb)) = (size(id), size(other)) {
                        scene.placements.insert(id, layout::refit(b, sb, sa));
                        scene.placements.insert(other, layout::refit(a, sa, sb));
                        scene.save_layout();
                    }
                }
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
            scene.request(Request::SetSound {
                to_headset: enabled,
            });
        }
        Action::SetMirror(enabled) => {
            if let Some(id) = scene.physical_id() {
                scene.mirror_wanted = enabled;
                scene.request(Request::ShowPhysical {
                    id: DisplayId(id),
                    shown: enabled,
                });
            }
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
            scene.pending_places = None;
            let shown_physical = scene
                .physical_id()
                .filter(|_| p.mirror)
                .map(DisplayId)
                .into_iter()
                .collect();
            scene.mirror_wanted = p.mirror;
            scene.request(Request::SetDisplays {
                modes: p.sizes.iter().map(|&(w, h)| Mode::new(w, h)).collect(),
                primary: Some(Primary::Index { index: p.primary }),
                shown_physical,
            });
            let s = &mut scene.settings;
            s.preset = p.settings.preset;
            s.curve = p.settings.curve;
            s.background = p.settings.background;
            s.cinema_screen = p.settings.cinema_screen;
            scene.set_cinema(p.settings.cinema);
            scene.settings.save(settings_path);
            set_passthrough(scene, passthrough);
            scene.active_profile = Some(i);
            let until = Instant::now() + Duration::from_secs(8);
            if p.places.is_empty() || p.settings.cinema {
                // From before places were kept: laid out by its preset.
                scene.relayout_until = Some(until);
                scene.arrange_all();
            } else {
                // Its own places, once the PC says which numbers the monitors have.
                scene.relayout_until = None;
                scene.pending_places = Some((p.places.clone(), until));
            }
            // The monitors changing is the answer: the menu gets out of the way.
            scene.panel_open = false;
        }
        Action::SaveProfile(i) => {
            let confirmed = scene
                .confirm_save
                .is_some_and(|(c, at)| c == i && at.elapsed() < CONFIRM_FOR);
            if confirmed {
                scene.confirm_save = None;
                scene.profiles[i] = scene.current_profile();
                profiles::save(&scene.profiles_path, &scene.profiles);
                scene.active_profile = Some(i);
            } else {
                scene.confirm_save = Some((i, Instant::now()));
            }
        }
        Action::AskMic => crate::mic::ask_again(),
        Action::ForgetPc(i) => {
            let confirmed = scene
                .confirm_forget
                .is_some_and(|(c, at)| c == i && at.elapsed() < CONFIRM_FOR);
            if !confirmed {
                scene.confirm_forget = Some((i, Instant::now()));
            } else if let Some((key, name)) = scene.paired.get(i).cloned() {
                scene.confirm_forget = None;
                let mut store = prostir_proto::secure::TrustStore::load(&scene.trust_file);
                match store.remove(&key) {
                    Ok(()) => {
                        info!("forgot PC {name:?}");
                        scene.steering.trust_changed();
                        // Connected to it: leave, and do not come back on our own.
                        if scene.host.as_deref() == Some(name.as_str()) {
                            scene.steering.forget_current();
                        }
                    }
                    Err(e) => scene.show_error(format!("{e}")),
                }
                scene.paired = store.entries().to_vec();
            }
        }
        Action::SetEncoding(change) => {
            // Shown at once; the PC confirms (or corrects) with its `Encoding`.
            if let Some(pc) = &mut scene.pc {
                let w = &mut pc.wanted;
                w.bitrate = change.bitrate.unwrap_or(w.bitrate);
                w.codec = change.codec.or(w.codec);
                w.software = change.software.unwrap_or(w.software);
                w.sharp = change.sharp.unwrap_or(w.sharp);
            }
            scene.request(Request::SetEncoding {
                bitrate: change.bitrate,
                codec: change.codec,
                software: change.software,
                sharp: change.sharp,
            });
        }
        Action::ShowTips => {
            scene.settings.intro_seen = false;
            scene.settings.save(settings_path);
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
                    scene.show_error(format!("«{typed}»: {what}"))
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
