//! Controller and hand input, mapped onto the same actions: a ray (aim pose),
//! "select" (trigger or pinch) and "grab" (grip or fist).

use anyhow::Result;
use glam::{Vec2, Vec3};
use openxr as xr;

use crate::layout::Ray;
use crate::xrutil::{Gfx, quat, vec3};

/// Analog press with hysteresis, so a half-pulled trigger does not flicker.
#[derive(Default, Clone, Copy)]
pub struct Button {
    pub down: bool,
    /// Went down during this frame.
    pub pressed: bool,
    /// Went up during this frame.
    pub released: bool,
}

impl Button {
    fn update(&mut self, value: f32) {
        let was = self.down;
        self.down = if was { value > 0.4 } else { value > 0.75 };
        self.pressed = self.down && !was;
        self.released = !self.down && was;
    }
}

#[derive(Default, Clone, Copy)]
pub struct HandState {
    pub ray: Option<Ray>,
    pub select: Button,
    pub grab: Button,
    pub stick: Vec2,
}

pub struct Controls {
    set: xr::ActionSet,
    hands: [xr::Path; 2],
    aim_spaces: [xr::Space; 2],
    grab: xr::Action<f32>,
    select: xr::Action<f32>,
    thumbstick: xr::Action<xr::Vector2f>,
    haptic: xr::Action<xr::Haptic>,
    menu: xr::Action<bool>,
    align: xr::Action<bool>,
    /// `/interaction_profiles/ext/hand_interaction_ext`, when hands are bound.
    hand_profile: Option<xr::Path>,
    /// Which sides are currently driven by tracked hands rather than controllers.
    tracked_hands: [bool; 2],
    /// Whether tracked hands may point and click; otherwise their input is ignored.
    pub hands_enabled: bool,
    pub state: [HandState; 2],
}

impl Controls {
    pub fn new(
        instance: &xr::Instance,
        session: &xr::Session<Gfx>,
        hand_interaction: bool,
    ) -> Result<Self> {
        let set = instance.create_action_set("monitors", "Monitors", 0)?;
        let hands = [
            instance.string_to_path("/user/hand/left")?,
            instance.string_to_path("/user/hand/right")?,
        ];
        let aim = set.create_action::<xr::Posef>("aim", "Aim", &hands)?;
        let grab = set.create_action::<f32>("grab", "Grab monitor", &hands)?;
        let select = set.create_action::<f32>("select", "Select", &hands)?;
        let thumbstick =
            set.create_action::<xr::Vector2f>("adjust", "Distance and size", &hands)?;
        let haptic = set.create_action::<xr::Haptic>("haptic", "Haptic", &hands)?;
        let menu = set.create_action::<bool>("menu", "Menu", &[])?;
        let align = set.create_action::<bool>("align", "Align monitors", &[])?;
        let p = |s: &str| instance.string_to_path(s);

        let mut touch = Vec::new();
        for side in ["left", "right"] {
            let path = |input: &str| p(&format!("/user/hand/{side}/{input}"));
            touch.push(xr::Binding::new(&aim, path("input/aim/pose")?));
            touch.push(xr::Binding::new(&grab, path("input/squeeze/value")?));
            touch.push(xr::Binding::new(&select, path("input/trigger/value")?));
            touch.push(xr::Binding::new(&thumbstick, path("input/thumbstick")?));
            touch.push(xr::Binding::new(&haptic, path("output/haptic")?));
        }
        touch.push(xr::Binding::new(
            &menu,
            p("/user/hand/left/input/menu/click")?,
        ));
        touch.push(xr::Binding::new(&menu, p("/user/hand/left/input/x/click")?));
        touch.push(xr::Binding::new(
            &align,
            p("/user/hand/right/input/a/click")?,
        ));
        instance.suggest_interaction_profile_bindings(
            p("/interaction_profiles/oculus/touch_controller")?,
            &touch,
        )?;

        let hand_profile = if hand_interaction {
            Some(p("/interaction_profiles/ext/hand_interaction_ext")?)
        } else {
            None
        };
        if let Some(profile) = hand_profile {
            let mut hand = Vec::new();
            for side in ["left", "right"] {
                let path = |input: &str| p(&format!("/user/hand/{side}/input/{input}"));
                hand.push(xr::Binding::new(&aim, path("aim/pose")?));
                hand.push(xr::Binding::new(&select, path("pinch_ext/value")?));
                hand.push(xr::Binding::new(&grab, path("grasp_ext/value")?));
            }
            instance.suggest_interaction_profile_bindings(profile, &hand)?;
        }

        session.attach_action_sets(&[&set])?;
        let aim_spaces = [
            aim.create_space(session, hands[0], xr::Posef::IDENTITY)?,
            aim.create_space(session, hands[1], xr::Posef::IDENTITY)?,
        ];
        Ok(Self {
            set,
            hands,
            aim_spaces,
            grab,
            select,
            thumbstick,
            haptic,
            menu,
            align,
            hand_profile,
            tracked_hands: [false; 2],
            hands_enabled: false,
            state: [HandState::default(); 2],
        })
    }

    /// Call when the runtime reports an interaction profile change (controllers
    /// picked up or put down).
    pub fn refresh_profiles(&mut self, session: &xr::Session<Gfx>) {
        for hand in 0..2 {
            let profile = session.current_interaction_profile(self.hands[hand]).ok();
            self.tracked_hands[hand] = profile.is_some() && profile == self.hand_profile;
        }
    }

    /// Reads all inputs for this frame.
    pub fn sync(
        &mut self,
        session: &xr::Session<Gfx>,
        base: &xr::Space,
        time: xr::Time,
    ) -> Result<()> {
        session.sync_actions(&[(&self.set).into()])?;
        // Cheap, and the runtime does not always announce the profile at start-up.
        self.refresh_profiles(session);
        for hand in 0..2 {
            let path = self.hands[hand];
            let location = self.aim_spaces[hand].locate(base, time)?;
            let tracked = location.location_flags.contains(
                xr::SpaceLocationFlags::POSITION_VALID | xr::SpaceLocationFlags::ORIENTATION_VALID,
            );
            let s = &mut self.state[hand];
            if self.tracked_hands[hand] && !self.hands_enabled {
                *s = HandState::default();
                continue;
            }
            s.ray = tracked.then(|| Ray {
                origin: vec3(location.pose.position),
                dir: quat(location.pose.orientation) * -Vec3::Z,
            });
            let value =
                |a: &xr::Action<f32>| a.state(session, path).map_or(0.0, |s| s.current_state);
            s.select.update(value(&self.select));
            s.grab.update(value(&self.grab));
            s.stick = self
                .thumbstick
                .state(session, path)
                .map_or(Vec2::ZERO, |s| {
                    Vec2::new(s.current_state.x, s.current_state.y)
                });
        }
        Ok(())
    }

    pub fn menu_pressed(&self, session: &xr::Session<Gfx>) -> bool {
        pressed_now(&self.menu, session)
    }

    pub fn align_pressed(&self, session: &xr::Session<Gfx>) -> bool {
        pressed_now(&self.align, session)
    }

    pub fn pulse(&self, session: &xr::Session<Gfx>, hand: usize) {
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
