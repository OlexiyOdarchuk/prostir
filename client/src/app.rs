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

use android_activity::{AndroidApp, MainEvent, PollEvent};
use anyhow::{Result, anyhow, bail};
use glam::Vec3;
use log::{error, info, warn};
use openxr as xr;
use openxr::sys::Handle as _;
use qmon_proto::{ClientMsg, MIRROR_ID};

use crate::input::Controls;
use crate::layout::{self, Cylinder, Grab, Hit, Placement, Ray};
use crate::net::{Feedback, NetEvent};
use crate::settings::{Background, Settings};
use crate::ui::{self, Action, MonitorEntry, Panel, UiState};
use crate::xrutil::{self, Gfx, ImageLayer, as_layer, pose, posef, vec3};
use crate::{background, decoder, egl, net, passthrough};

/// Grab id for the menu panel (monitor ids are 0..=MIRROR_ID).
const PANEL_ID: u8 = 250;
const PANEL_WIDTH: f32 = 0.62;
const BUTTON_SIZE: f32 = 0.075;
const CURSOR_PX: u32 = 64;
const BUTTON_PX: u32 = 128;
const ERROR_SHOWN_FOR: Duration = Duration::from_secs(8);

#[unsafe(no_mangle)]
pub fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(if cfg!(debug_assertions) {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            })
            .with_tag("questmon"),
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
}

/// Everything shown in the headset and how it is arranged.
struct Scene {
    monitors: HashMap<u8, Monitor>,
    placements: HashMap<u8, Placement>,
    primary: Option<u8>,
    mirror_wanted: bool,
    settings: Settings,
    curve_supported: bool,
    panel_open: bool,
    panel_place: Placement,
    button_place: Option<Placement>,
    grabs: [Option<Grab>; 2],
    host: Option<String>,
    feedback: Option<Feedback>,
    error: Option<(String, Instant)>,
    layout_path: PathBuf,
    settings_path: PathBuf,
}

impl Scene {
    fn quest_sizes(&self) -> Vec<(u8, u32, u32)> {
        let mut v: Vec<_> = self
            .monitors
            .iter()
            .filter(|(id, _)| **id != MIRROR_ID)
            .map(|(id, m)| (*id, m.width_px, m.height_px))
            .collect();
        v.sort_by_key(|m| m.0);
        v
    }

    fn mirror_size(&self) -> Option<(u8, u32, u32)> {
        self.monitors
            .get(&MIRROR_ID)
            .map(|m| (MIRROR_ID, m.width_px, m.height_px))
    }

    /// Lays everything out by the chosen preset.
    fn arrange_all(&mut self) {
        let mirror_place = self.placements.get(&MIRROR_ID).copied();
        self.placements = layout::arrange(
            self.settings.preset,
            &self.quest_sizes(),
            self.primary,
            self.mirror_size(),
        );
        // Keep the mirror's spot while the laptop lid is closed.
        if let (Some(p), false) = (mirror_place, self.placements.contains_key(&MIRROR_ID)) {
            self.placements.insert(MIRROR_ID, p);
        }
        self.grabs = [None, None];
        layout::save(&self.layout_path, &self.placements);
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

    /// Puts the menu button just under the primary monitor's bottom-left corner.
    fn place_button(&mut self) {
        let anchor = self
            .primary
            .filter(|id| self.monitors.contains_key(id))
            .or_else(|| self.quest_sizes().first().map(|m| m.0));
        self.button_place = anchor.and_then(|id| {
            let p = self.placements.get(&id)?;
            let h = self.height_of(id);
            let offset = Vec3::new(
                -p.width / 2.0 + BUTTON_SIZE,
                -h / 2.0 - BUTTON_SIZE * 0.9,
                0.02,
            );
            Some(Placement {
                position: p.position + p.rotation * offset,
                rotation: p.rotation,
                width: BUTTON_SIZE,
            })
        });
    }

    fn open_panel(&mut self, head: Vec3, forward: Vec3) {
        let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(-Vec3::Z);
        let position = head + flat * 0.7 - Vec3::Y * 0.12;
        self.panel_place = Placement {
            position,
            rotation: layout::facing(position, head),
            width: PANEL_WIDTH,
        };
        self.panel_open = true;
    }

    fn send(&mut self, msg: ClientMsg) {
        let sent = self.feedback.as_ref().map(|f| f.send(&msg));
        match sent {
            Some(Ok(())) => {}
            Some(Err(e)) => self.show_error(format!("Не вдалося надіслати на ПК: {e}")),
            None => self.show_error("Немає з'єднання з ПК".into()),
        }
    }

    fn show_error(&mut self, text: String) {
        warn!("{text}");
        self.error = Some((text, Instant::now()));
    }

    fn ui_state(&self, room: bool, panorama: bool) -> UiState {
        UiState {
            host: self.host.clone(),
            monitors: self
                .quest_sizes()
                .into_iter()
                .map(|(id, width, height)| MonitorEntry { id, width, height })
                .collect(),
            primary: self.primary,
            mirror_wanted: self.mirror_wanted,
            mirror_active: self.monitors.contains_key(&MIRROR_ID),
            settings: self.settings,
            room_available: room,
            panorama_available: panorama,
            curve_available: self.curve_supported,
            error: self
                .error
                .as_ref()
                .filter(|(_, at)| at.elapsed() < ERROR_SHOWN_FOR)
                .map(|(t, _)| t.clone()),
        }
    }
}

/// The user's own panorama (`background.jpg`/`.png` next to `host.txt`), else the night sky.
fn load_panorama(dir: Option<PathBuf>) -> (u32, u32, Vec<u8>) {
    let custom = dir.and_then(|d| {
        ["background.jpg", "background.jpeg", "background.png"]
            .iter()
            .map(|n| d.join(n))
            .find(|p| p.exists())
    });
    if let Some(path) = custom {
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
    (
        background::WIDTH,
        background::HEIGHT,
        background::night_sky(),
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
    let instance = entry.create_instance(
        &xr::ApplicationInfo {
            application_name: "Quest Monitors",
            application_version: 1,
            engine_name: "questmon",
            engine_version: 1,
            api_version: xr::Version::new(1, 0, 0),
        },
        &exts,
        &[],
        &platform,
    )?;
    info!(
        "cylinder layers: {}, equirect: {}, hand interaction: {}",
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
    let settings = Settings::load(&settings_path);

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

    let format = xrutil::color_format(&session);
    let mut panel_layer = ImageLayer::new(&session, format, ui::WIDTH, ui::HEIGHT, false)?;
    let mut button_layer = ImageLayer::new(&session, format, BUTTON_PX, BUTTON_PX, false)?;
    let mut cursor_layer = ImageLayer::new(&session, format, CURSOR_PX, CURSOR_PX, true)?;
    cursor_layer.upload(&gl, &ui::cursor_image(CURSOR_PX))?;
    let mut button_hovered = false;
    button_layer.upload(&gl, &ui::menu_button_image(BUTTON_PX, false))?;
    let mut panel = Panel::new();
    let mut pano_layer: Option<ImageLayer> = None;
    let mut pano_job: Option<JoinHandle<(u32, u32, Vec<u8>)>> = None;
    let external_dir = app.external_data_path();

    let (net_tx, net_rx) = mpsc::channel();
    net::spawn(
        net_tx,
        net::Discovery {
            host_file: external_dir.as_ref().map(|p| p.join("host.txt")),
            last_host_file: data_dir.join("last_host"),
        },
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
        placements: layout::load(&layout_path),
        primary: None,
        mirror_wanted: true,
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
        feedback: None,
        error: None,
        layout_path,
        settings_path,
    };

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
        app.poll_events(Some(timeout), |event| {
            if let PollEvent::Main(MainEvent::Destroy) = event {
                destroy = true;
            }
        });
        if destroy {
            break;
        }

        while let Some(event) = instance.poll_event(&mut event_storage)? {
            match event {
                xr::Event::SessionStateChanged(e) => {
                    info!("session state: {:?}", e.state());
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

        // Panorama: generated or decoded off the main thread, uploaded once.
        if scene.settings.background == Background::Panorama
            && exts.khr_composition_layer_equirect2
            && pano_layer.is_none()
            && pano_job.is_none()
        {
            let dir = external_dir.clone();
            pano_job = Some(std::thread::spawn(move || load_panorama(dir)));
        }
        if pano_job.as_ref().is_some_and(|j| j.is_finished()) {
            match pano_job.take().unwrap().join() {
                Ok((w, h, pixels)) => {
                    let mut layer = ImageLayer::new(&session, format, w, h, true)?;
                    layer.upload(&gl, &pixels)?;
                    pano_layer = Some(layer);
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

        if controls.menu_pressed(&session) {
            if scene.panel_open {
                scene.panel_open = false;
            } else {
                scene.open_panel(head, forward);
            }
        }
        if controls.align_pressed(&session) {
            scene.arrange_all();
        }
        scene.place_button();

        // Pointing, clicking and dragging.
        let picks: [Option<(Target, Hit)>; 2] =
            std::array::from_fn(|h| controls.state[h].ray.and_then(|r| scene.pick(r)));
        let panel_point = |hit: &Hit| {
            let x = (hit.local.x / PANEL_WIDTH + 0.5) * ui::WIDTH as f32;
            let y = (0.5 - hit.local.y / panel_height()) * ui::HEIGHT as f32;
            (x, y)
        };
        let hover = [1, 0].into_iter().find_map(|h| match picks[h] {
            Some((Target::Panel, hit)) => Some(panel_point(&hit)),
            _ => None,
        });
        panel.hover(hover);
        let over_button = picks
            .iter()
            .any(|p| matches!(p, Some((Target::MenuButton, _))));
        if over_button != button_hovered {
            button_hovered = over_button;
            button_layer.upload(&gl, &ui::menu_button_image(BUTTON_PX, over_button))?;
        }

        #[allow(clippy::needless_range_loop)] // `hand` indexes several per-hand arrays
        for hand in 0..2 {
            let input = controls.state[hand];
            if input.select.pressed {
                match picks[hand] {
                    Some((Target::Panel, hit)) => {
                        panel.hover(Some(panel_point(&hit)));
                        if let Some(action) = panel.hovered_action() {
                            controls.pulse(&session, hand);
                            handle_action(&mut scene, action, &mut passthrough);
                        }
                    }
                    Some((Target::MenuButton, _)) => {
                        controls.pulse(&session, hand);
                        if scene.panel_open {
                            scene.panel_open = false;
                        } else {
                            scene.open_panel(head, forward);
                        }
                    }
                    _ => {}
                }
            }

            if input.grab.released && scene.grabs[hand].take().is_some() {
                layout::save(&scene.layout_path, &scene.placements);
            }
            let Some(ray) = input.ray else { continue };
            if let Some(mut grab) = scene.grabs[hand] {
                let target = if grab.id == PANEL_ID {
                    Some(&mut scene.panel_place)
                } else {
                    scene.placements.get_mut(&grab.id)
                };
                if let Some(p) = target {
                    grab.update(p, ray, head, input.stick, dt);
                    scene.grabs[hand] = Some(grab);
                }
            } else if input.grab.pressed {
                let other = scene.grabs[1 - hand].map(|g| g.id);
                let grabbed = match picks[hand] {
                    Some((Target::Panel, hit)) if other != Some(PANEL_ID) => {
                        Some(Grab::start(PANEL_ID, hit.t, hit.local, &scene.panel_place))
                    }
                    Some((Target::Monitor(id), hit)) if other != Some(id) => {
                        Some(Grab::start(id, hit.t, hit.local, &scene.placements[&id]))
                    }
                    _ => None,
                };
                if grabbed.is_some() {
                    controls.pulse(&session, hand);
                    scene.grabs[hand] = grabbed;
                }
            }
        }

        // Menu contents.
        if scene.panel_open {
            let ui_state =
                scene.ui_state(passthrough.is_some(), exts.khr_composition_layer_equirect2);
            panel.set_state(&ui_state);
            if let Some(pixels) = panel.render_if_dirty() {
                panel_layer.upload(&gl, pixels)?;
            }
        }

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

        let passthrough_layer = passthrough.as_ref().filter(|pt| pt.running()).map(|pt| {
            xr::sys::CompositionLayerPassthroughFB {
                ty: xr::sys::CompositionLayerPassthroughFB::TYPE,
                next: ptr::null(),
                flags: xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA,
                space: xr::sys::Space::NULL,
                layer_handle: pt.layer(),
            }
        });
        let pano = pano_layer
            .as_ref()
            .filter(|_| scene.settings.background == Background::Panorama)
            .map(|l| xr::sys::CompositionLayerEquirect2KHR {
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
        let mut order: Vec<(f32, u8)> = scene
            .monitors
            .keys()
            .filter_map(|id| {
                let d = scene.placements.get(id)?.position.distance(head);
                Some((if grabbed.contains(id) { -1.0 } else { d }, *id))
            })
            .collect();
        order.sort_by(|a, b| b.0.total_cmp(&a.0));
        let curve = scene.curve_radius();
        let mut quads = Vec::new();
        let mut cylinders = Vec::new();
        for (_, id) in &order {
            let m = &scene.monitors[id];
            let p = &scene.placements[id];
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
                    next: video_next,
                    layer_flags: xr::CompositionLayerFlags::EMPTY,
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
                    next: video_next,
                    layer_flags: xr::CompositionLayerFlags::EMPTY,
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
        let cursors: Vec<xr::sys::CompositionLayerQuad> = picks
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

        let mut layers: Vec<&xr::CompositionLayerBase<Gfx>> = Vec::with_capacity(16);
        if let Some(l) = &passthrough_layer {
            layers.push(as_layer(l));
        } else if let Some(l) = &pano {
            layers.push(as_layer(l));
        }
        layers.extend(cylinders.iter().map(as_layer));
        layers.extend(quads.iter().map(as_layer));
        layers.extend(button.iter().map(as_layer));
        layers.extend(panel_quad.iter().map(as_layer));
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
        NetEvent::Connected(addr, feedback) => {
            info!("connected to {addr}");
            scene.host = Some(addr.ip().to_string());
            scene.feedback = Some(feedback);
        }
        NetEvent::Disconnected => {
            scene.host = None;
            scene.feedback = None;
        }
        NetEvent::PrimaryChanged(id) => scene.primary = Some(id),
        NetEvent::MirrorWanted(enabled) => scene.mirror_wanted = enabled,
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
                    if !scene.placements.contains_key(&id) {
                        if id == MIRROR_ID {
                            // Only the mirror is new: put it under the primary monitor.
                            let all = layout::arrange(
                                scene.settings.preset,
                                &scene.quest_sizes(),
                                scene.primary,
                                scene.mirror_size(),
                            );
                            if let Some(p) = all.get(&MIRROR_ID) {
                                scene.placements.insert(MIRROR_ID, *p);
                            }
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

fn handle_action(
    scene: &mut Scene,
    action: Action,
    passthrough: &mut Option<passthrough::Passthrough>,
) {
    let settings_path: &Path = &scene.settings_path.clone();
    match action {
        Action::Add { width, height } => scene.send(ClientMsg::AddMonitor { width, height }),
        Action::Remove(id) => scene.send(ClientMsg::RemoveMonitor { id }),
        Action::MakePrimary(id) => scene.send(ClientMsg::SetPrimary { id }),
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
        }
        Action::Background(background) => {
            scene.settings.background = background;
            scene.settings.save(settings_path);
            if let Some(pt) = passthrough
                && let Err(e) = pt.set_running(background == Background::Room)
            {
                scene.show_error(format!("Камери недоступні: {e:#}"));
            }
        }
        Action::Close => scene.panel_open = false,
    }
}
