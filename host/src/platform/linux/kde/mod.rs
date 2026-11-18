//! KDE Plasma: KWin makes and streams the outputs, KScreen places them, the Plasma
//! shell keeps its panel and wallpapers.

pub mod kscreen;
pub mod plasma;
pub mod virtual_output;
pub mod windows;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use log::info;

use crate::platform::Geometry;
use virtual_output::{Screencast, VirtualOutput};

/// The KDE side of [`super::desktop::Desktop`].
pub struct Kde {
    pub screencast: Screencast,
    /// When Plasma was last nudged to follow the primary output.
    primary_nudged: Option<Instant>,
    /// Last time the laptop panel was switched back on (to avoid fighting the user).
    woke_display_at: Option<Instant>,
}

impl Kde {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            screencast: Screencast::connect()?,
            primary_nudged: None,
            woke_display_at: None,
        })
    }

    /// Moves outputs to `positions` and makes `primary` the primary output. Only what
    /// differs from `current` is sent.
    pub fn place(
        &self,
        positions: &[(String, i32, i32)],
        primary: Option<&str>,
        current: &HashMap<String, Geometry>,
    ) {
        let mut settings: Vec<String> = positions
            .iter()
            .filter(|(name, x, y)| current.get(name).is_none_or(|g| (g.x, g.y) != (*x, *y)))
            .map(|(name, x, y)| format!("output.{name}.position.{x},{y}"))
            .collect();
        if let Some(p) = primary
            && current.get(p).is_none_or(|g| g.priority != Some(1))
        {
            settings.push(format!("output.{p}.priority.1"));
        }
        kscreen::apply(&settings);
    }

    /// Makes sure KDE's primary output (where the Plasma panel lives) is `ours`. Right
    /// after outputs come and go (a profile switch) KDE can end up with another one.
    pub fn keep_primary(&mut self, output: &VirtualOutput) {
        let geometries = kscreen::geometries();
        let ours = output.output_name();
        let kde_primary = geometries
            .iter()
            .filter(|(_, g)| g.enabled && g.w > 0)
            .min_by_key(|(_, g)| g.priority.unwrap_or(u32::MAX))
            .map(|(name, _)| name.clone());
        let Some(g) = geometries.get(ours) else {
            return;
        };
        if kde_primary.as_deref() != Some(ours) {
            info!(
                "KDE's primary output is {}, not {ours}: setting it again",
                kde_primary.as_deref().unwrap_or("none")
            );
            output.make_primary();
            return;
        }
        // KDE agrees; Plasma may still keep the panel on the former primary.
        let Some(first) = plasma::first_screen() else {
            return;
        };
        if first == (g.x, g.y, g.w, g.h)
            || self
                .primary_nudged
                .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        let other = geometries
            .iter()
            .find(|(name, g)| g.enabled && g.w > 0 && name.as_str() != ours)
            .map(|(name, _)| name.clone());
        if let Some(other) = other {
            info!("Plasma's first screen is not {ours}: nudging it to follow");
            self.primary_nudged = Some(Instant::now());
            plasma::nudge_primary(ours.to_owned(), other);
        }
    }

    /// KDE sometimes leaves the laptop panel off after the lid is opened again while
    /// virtual monitors exist. With the lid open, switch it on (for the mirror).
    pub fn wake_physical_display(&mut self) {
        if self
            .woke_display_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(30))
            || !crate::platform::linux::system::lid_open()
        {
            return;
        }
        let off = kscreen::disabled_physical_outputs();
        if off.is_empty() {
            return;
        }
        self.woke_display_at = Some(Instant::now());
        info!("lid is open but {off:?} is off: switching it on for the mirror");
        let settings: Vec<String> = off.iter().map(|n| format!("output.{n}.enable")).collect();
        kscreen::apply(&settings);
    }
}
