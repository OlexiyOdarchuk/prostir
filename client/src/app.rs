//! OpenXR main loop: shows each virtual KDE monitor as an OpenXR quad layer.
//!
//! Video frames are decoded by MediaCodec directly into Android surfaces owned
//! by the Quest compositor (XR_KHR_android_surface_swapchain), so pixels never
//! pass through this app's GPU code and text stays as sharp as the compositor
//! can make it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use android_activity::{AndroidApp, MainEvent, PollEvent};
use anyhow::{Context, Result, anyhow, bail};
use glam::{Quat, Vec2, Vec3};
use log::{error, info, warn};
use ndk::native_window::NativeWindow;
use openxr as xr;
use openxr::sys::Handle as _;

use crate::layout::{self, Grab, Placement, Ray};
use crate::net::NetEvent;
use crate::{decoder, egl, net, passthrough};

type Gfx = xr::OpenGlEs;

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

/// One virtual monitor: the compositor surface it is decoded into and its decoder thread.
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

fn create_surface_swapchain(
    session: &xr::Session<Gfx>,
    vm: &jni::JavaVM,
    width: u32,
    height: u32,
) -> Result<(xr::Swapchain<Gfx>, NativeWindow)> {
    let ext = session
        .instance()
        .exts()
        .khr_android_surface_swapchain
        .as_ref()
        .context("XR_KHR_android_surface_swapchain is not enabled")?;
    // Meta's runtime rejects surface swapchains with a format or non-zero counts
    // ("info format validation failure"): MediaCodec decides those.
    let info = xr::sys::SwapchainCreateInfo {
        ty: xr::sys::SwapchainCreateInfo::TYPE,
        next: ptr::null(),
        create_flags: xr::SwapchainCreateFlags::EMPTY,
        usage_flags: xr::SwapchainUsageFlags::SAMPLED | xr::SwapchainUsageFlags::COLOR_ATTACHMENT,
        format: 0,
        sample_count: 0,
        width,
        height,
        face_count: 0,
        array_size: 0,
        mip_count: 0,
    };
    let mut handle = xr::sys::Swapchain::NULL;
    let mut surface = ptr::null_mut();
    // SAFETY: valid session and create info; out-pointers live on the stack.
    let result = unsafe {
        (ext.create_swapchain_android_surface)(session.as_raw(), &info, &mut handle, &mut surface)
    };
    if result != xr::sys::Result::SUCCESS {
        bail!("xrCreateSwapchainAndroidSurfaceKHR failed: {result:?}");
    }
    // SAFETY: the handle was just created for this session and is owned by the wrapper.
    let swapchain = unsafe { xr::Swapchain::from_raw(session.clone(), handle) };
    let env = vm.attach_current_thread_permanently()?;
    // SAFETY: `surface` is a valid android.view.Surface reference returned by the runtime.
    let window = unsafe { NativeWindow::from_surface(env.get_raw().cast(), surface.cast()) }
        .context("ANativeWindow_fromSurface failed")?;
    Ok((swapchain, window))
}

/// Views a raw `XrCompositionLayer*` struct as the base type `FrameStream::end` takes.
fn as_layer<'a, T>(layer: &'a T) -> &'a xr::CompositionLayerBase<'a, Gfx> {
    // SAFETY: only called with XrCompositionLayer* structs, which all begin with the
    // common layer header; CompositionLayerBase is a transparent wrapper over it.
    unsafe { &*(layer as *const T).cast() }
}

fn vec3(v: xr::Vector3f) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn quat(q: xr::Quaternionf) -> Quat {
    Quat::from_xyzw(q.x, q.y, q.z, q.w)
}

fn posef(p: &Placement) -> xr::Posef {
    let (q, v) = (p.rotation, p.position);
    xr::Posef {
        orientation: xr::Quaternionf {
            x: q.x,
            y: q.y,
            z: q.z,
            w: q.w,
        },
        position: xr::Vector3f {
            x: v.x,
            y: v.y,
            z: v.z,
        },
    }
}

struct Controls {
    set: xr::ActionSet,
    hands: [xr::Path; 2],
    aim_spaces: [xr::Space; 2],
    squeeze: xr::Action<f32>,
    thumbstick: xr::Action<xr::Vector2f>,
    haptic: xr::Action<xr::Haptic>,
    reset_layout: xr::Action<bool>,
    toggle_passthrough: xr::Action<bool>,
}

impl Controls {
    fn new(instance: &xr::Instance, session: &xr::Session<Gfx>) -> Result<Self> {
        let set = instance.create_action_set("monitors", "Monitors", 0)?;
        let hands = [
            instance.string_to_path("/user/hand/left")?,
            instance.string_to_path("/user/hand/right")?,
        ];
        let aim = set.create_action::<xr::Posef>("aim", "Aim", &hands)?;
        let squeeze = set.create_action::<f32>("grab", "Grab monitor", &hands)?;
        let thumbstick =
            set.create_action::<xr::Vector2f>("adjust", "Distance and size", &hands)?;
        let haptic = set.create_action::<xr::Haptic>("haptic", "Haptic", &hands)?;
        let reset_layout = set.create_action::<bool>("reset_layout", "Reset layout", &[])?;
        let toggle_passthrough =
            set.create_action::<bool>("toggle_passthrough", "Toggle passthrough", &[])?;
        let p = |s: &str| instance.string_to_path(s);
        instance.suggest_interaction_profile_bindings(
            p("/interaction_profiles/oculus/touch_controller")?,
            &[
                xr::Binding::new(&aim, p("/user/hand/left/input/aim/pose")?),
                xr::Binding::new(&aim, p("/user/hand/right/input/aim/pose")?),
                xr::Binding::new(&squeeze, p("/user/hand/left/input/squeeze/value")?),
                xr::Binding::new(&squeeze, p("/user/hand/right/input/squeeze/value")?),
                xr::Binding::new(&thumbstick, p("/user/hand/left/input/thumbstick")?),
                xr::Binding::new(&thumbstick, p("/user/hand/right/input/thumbstick")?),
                xr::Binding::new(&haptic, p("/user/hand/left/output/haptic")?),
                xr::Binding::new(&haptic, p("/user/hand/right/output/haptic")?),
                xr::Binding::new(&reset_layout, p("/user/hand/right/input/a/click")?),
                xr::Binding::new(&toggle_passthrough, p("/user/hand/left/input/x/click")?),
            ],
        )?;
        session.attach_action_sets(&[&set])?;
        let aim_spaces = [
            aim.create_space(session, hands[0], xr::Posef::IDENTITY)?,
            aim.create_space(session, hands[1], xr::Posef::IDENTITY)?,
        ];
        Ok(Self {
            set,
            hands,
            aim_spaces,
            squeeze,
            thumbstick,
            haptic,
            reset_layout,
            toggle_passthrough,
        })
    }

    fn pulse(&self, session: &xr::Session<Gfx>, hand: usize) {
        let vibration = xr::HapticVibration::new()
            .amplitude(0.4)
            .duration(xr::Duration::from_nanos(25_000_000))
            .frequency(xr::FREQUENCY_UNSPECIFIED);
        let _ = self
            .haptic
            .apply_feedback(session, self.hands[hand], &vibration);
    }
}

fn pressed_now(action: &xr::Action<bool>, session: &xr::Session<Gfx>) -> bool {
    action
        .state(session, xr::Path::NULL)
        .is_ok_and(|s| s.is_active && s.changed_since_last_sync && s.current_state)
}

fn monitor_sizes(monitors: &HashMap<u8, Monitor>) -> Vec<(u8, u32, u32)> {
    monitors
        .iter()
        .map(|(id, m)| (*id, m.width_px, m.height_px))
        .collect()
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

    let data_dir: PathBuf = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    // Passthrough keeps the cameras busy and costs battery, so it starts off unless
    // it was left on last time.
    let passthrough_path = data_dir.join("passthrough");
    let passthrough_wanted =
        std::fs::read_to_string(&passthrough_path).is_ok_and(|s| s.trim() == "1");
    let mut passthrough = if exts.fb_passthrough {
        passthrough::Passthrough::new(&session, passthrough_wanted)
            .inspect_err(|e| warn!("passthrough unavailable: {e:#}"))
            .ok()
    } else {
        None
    };

    let local =
        session.create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)?;
    let view = session.create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)?;
    let controls = Controls::new(&instance, &session)?;

    let (net_tx, net_rx) = mpsc::channel();
    net::spawn(
        net_tx,
        net::Discovery {
            host_file: app.external_data_path().map(|p| p.join("host.txt")),
            last_host_file: data_dir.join("last_host"),
        },
    );

    // SAFETY: the VM pointer is valid for the life of the process.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }?;
    let layout_path = data_dir.join("layout.txt");
    let mut placements = layout::load(&layout_path);
    let mut monitors: HashMap<u8, Monitor> = HashMap::new();
    let mut grabs: [Option<Grab>; 2] = [None, None];
    let mut squeeze_down = [false; 2];
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
            match event {
                NetEvent::Connected(addr) => info!("connected to {addr}"),
                NetEvent::Disconnected => info!("disconnected"),
                NetEvent::StreamStart {
                    id,
                    width,
                    height,
                    codec,
                    frames,
                } => {
                    monitors.remove(&id);
                    match create_surface_swapchain(&session, &vm, width, height) {
                        Ok((swapchain, window)) => {
                            let stop = Arc::new(AtomicBool::new(false));
                            let decoder = decoder::spawn(
                                id,
                                codec,
                                width,
                                height,
                                window,
                                frames,
                                stop.clone(),
                            );
                            monitors.insert(
                                id,
                                Monitor {
                                    width_px: width,
                                    height_px: height,
                                    decoder: Some(decoder),
                                    stop,
                                    swapchain,
                                },
                            );
                            if let std::collections::hash_map::Entry::Vacant(slot) =
                                placements.entry(id)
                                && let Some(p) =
                                    layout::default_layout(&monitor_sizes(&monitors)).remove(&id)
                            {
                                slot.insert(p);
                            }
                        }
                        Err(e) => error!("monitor {id}: {e:#}"),
                    }
                }
                NetEvent::StreamEnd { id } => {
                    monitors.remove(&id);
                    for g in &mut grabs {
                        if g.is_some_and(|g| g.id == id) {
                            *g = None;
                        }
                    }
                }
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

        session.sync_actions(&[(&controls.set).into()])?;
        let head = view
            .locate(&local, time)
            .map(|l| vec3(l.pose.position))
            .unwrap_or(Vec3::ZERO);

        if pressed_now(&controls.reset_layout, &session) {
            placements = layout::default_layout(&monitor_sizes(&monitors));
            grabs = [None, None];
            layout::save(&layout_path, &placements);
        }
        if let Some(pt) = &mut passthrough
            && pressed_now(&controls.toggle_passthrough, &session)
        {
            let on = !pt.running();
            match pt.set_running(on) {
                Ok(()) => {
                    let _ = std::fs::write(&passthrough_path, if on { "1" } else { "0" });
                }
                Err(e) => warn!("{e:#}"),
            }
        }

        for hand in 0..2 {
            let path = controls.hands[hand];
            let location = controls.aim_spaces[hand].locate(&local, time)?;
            let tracked = location.location_flags.contains(
                xr::SpaceLocationFlags::POSITION_VALID | xr::SpaceLocationFlags::ORIENTATION_VALID,
            );
            let squeeze = controls
                .squeeze
                .state(&session, path)
                .map(|s| s.current_state)
                .unwrap_or(0.0);
            let stick = controls
                .thumbstick
                .state(&session, path)
                .map(|s| Vec2::new(s.current_state.x, s.current_state.y))
                .unwrap_or(Vec2::ZERO);
            let was_down = squeeze_down[hand];
            squeeze_down[hand] = if was_down {
                squeeze > 0.4
            } else {
                squeeze > 0.7
            };
            if !tracked {
                continue;
            }
            let ray = Ray {
                origin: vec3(location.pose.position),
                dir: quat(location.pose.orientation) * -Vec3::Z,
            };

            if !squeeze_down[hand] {
                if grabs[hand].take().is_some() {
                    layout::save(&layout_path, &placements);
                }
                continue;
            }
            if let Some(grab) = &mut grabs[hand] {
                if let Some(p) = placements.get_mut(&grab.id) {
                    grab.update(p, ray, head, stick, dt);
                }
            } else if !was_down {
                let other = grabs[1 - hand].map(|g| g.id);
                let target = monitors
                    .iter()
                    .filter(|(id, _)| Some(**id) != other)
                    .filter_map(|(id, m)| {
                        let p = placements.get(id)?;
                        let (t, local_hit) = layout::hit(ray, p, p.width * m.aspect())?;
                        Some((t, *id, local_hit))
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0));
                if let Some((t, id, local_hit)) = target {
                    grabs[hand] = Some(Grab::start(id, t, local_hit, &placements[&id]));
                    controls.pulse(&session, hand);
                }
            }
        }

        // Far monitors first so nearer (and grabbed) ones draw on top.
        let grabbed: Vec<u8> = grabs.iter().flatten().map(|g| g.id).collect();
        let mut order: Vec<(f32, u8)> = monitors
            .keys()
            .filter_map(|id| {
                let d = placements.get(id)?.position.distance(head);
                Some((if grabbed.contains(id) { -1.0 } else { d }, *id))
            })
            .collect();
        order.sort_by(|a, b| b.0.total_cmp(&a.0));

        // Desktop text is denser than the display, so ask the compositor to supersample
        // and sharpen the quads instead of shimmering through plain bilinear sampling.
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
        let quad_next = if exts.fb_composition_layer_image_layout {
            (&flip as *const xr::sys::CompositionLayerImageLayoutFB).cast()
        } else {
            quality_next
        };
        let quads: Vec<xr::sys::CompositionLayerQuad> = order
            .iter()
            .map(|(_, id)| {
                let m = &monitors[id];
                let p = &placements[id];
                xr::sys::CompositionLayerQuad {
                    ty: xr::sys::CompositionLayerQuad::TYPE,
                    next: quad_next,
                    layer_flags: xr::CompositionLayerFlags::EMPTY,
                    space: local.as_raw(),
                    eye_visibility: xr::EyeVisibility::BOTH,
                    sub_image: xr::sys::SwapchainSubImage {
                        swapchain: m.swapchain.as_raw(),
                        image_rect: xr::Rect2Di {
                            offset: xr::Offset2Di { x: 0, y: 0 },
                            extent: xr::Extent2Di {
                                width: m.width_px as i32,
                                height: m.height_px as i32,
                            },
                        },
                        image_array_index: 0,
                    },
                    pose: posef(p),
                    size: xr::Extent2Df {
                        width: p.width,
                        height: p.width * m.aspect(),
                    },
                }
            })
            .collect();
        let passthrough_layer = passthrough.as_ref().filter(|pt| pt.running()).map(|pt| {
            xr::sys::CompositionLayerPassthroughFB {
                ty: xr::sys::CompositionLayerPassthroughFB::TYPE,
                next: ptr::null(),
                flags: xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA,
                space: xr::sys::Space::NULL,
                layer_handle: pt.layer(),
            }
        });
        let mut layers: Vec<&xr::CompositionLayerBase<Gfx>> = Vec::with_capacity(quads.len() + 1);
        if let Some(l) = &passthrough_layer {
            layers.push(as_layer(l));
        }
        layers.extend(quads.iter().map(as_layer));
        frame_stream.end(time, xr::EnvironmentBlendMode::OPAQUE, &layers)?;
    }

    info!("exiting");
    drop(monitors);
    Ok(())
}
