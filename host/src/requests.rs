//! The headset's requests: each changes the PC's state and gets exactly one outcome.
//! The parts it changed go out before the reply (see `publish`). A request this
//! headset has no right to gets `NOT_ALLOWED`; arguments out of range get their own
//! code, and nothing changes. A request that makes displays answers with all it made,
//! also those that did not start: they stand in the list as `Failed`, keeping their
//! place.

use anyhow::{Context, Result, bail};
use log::info;
use prostir_proto::Codec;
use prostir_proto::request::{Outcome, Primary, Request};
use prostir_proto::state::{Access, ErrorCode, HeadsetCaps, Limits, Mode, caps, perms};

use crate::displays::Displays;
use crate::errors::{self, Counted, UserError};
use crate::keep_screens;
use crate::platform::MonitorSpec;
use crate::screen::ScreenId;
use crate::store::Store;

/// Who asks: what the headset can do, what the PC allows it, its rights.
pub struct Asker<'a> {
    pub caps: &'a HeadsetCaps,
    pub limits: &'a Limits,
    pub access: Access,
}

/// The rights a request needs.
fn needs(request: &Request) -> u32 {
    match request {
        Request::AddDisplay { .. }
        | Request::RemoveDisplay { .. }
        | Request::SetDisplays { .. }
        | Request::SetPrimary { .. }
        | Request::ShowPhysical { .. }
        | Request::SetEncoding { .. }
        | Request::SetSound { .. } => perms::CHANGES,
        Request::DetachWindow => perms::CHANGES | perms::PRIVATE,
        Request::WindowHere { .. } => perms::INPUT,
        Request::Screenshot { .. } => perms::PRIVATE,
        Request::FindMouse => 0,
    }
}

/// Carries out `request` for `asker`: its outcome.
pub fn handle(
    request: Request,
    displays: &mut Displays,
    store: &mut Store,
    asker: &Asker,
) -> Outcome {
    if !asker.access.may(needs(&request)) {
        info!("the headset has no right to {request:?}");
        return Outcome::failed(ErrorCode::NOT_ALLOWED, &UserError::NotAllowed.to_string());
    }
    carry_out(request, displays, store, asker).unwrap_or_else(|e| errors::outcome(&e))
}

fn carry_out(
    request: Request,
    displays: &mut Displays,
    store: &mut Store,
    asker: &Asker,
) -> Result<Outcome> {
    let limits = asker.limits;
    Ok(match request {
        Request::AddDisplay { mode } => {
            let spec = checked(mode, limits)?;
            let max = usize::from(limits.virtual_max);
            if displays.monitor_count() >= max {
                bail!(UserError::Limit {
                    max,
                    of: Counted::Monitors
                });
            }
            streams_within(displays.stream_count() + 1, limits)?;
            let id = store.new_id();
            displays.add(id, spec, max)?;
            info!("headset added {id}");
            displays.force_keyframe(id);
            keep_screens(store, displays, true);
            displays.sync_wallpapers();
            Outcome::Added { id: id.wire() }
        }
        Request::RemoveDisplay { id } => {
            let id = ScreenId::from_wire(id);
            let monitor = displays.is_monitor(id);
            displays.remove(id)?;
            info!("headset removed {id}");
            keep_screens(store, displays, monitor);
            Outcome::Done
        }
        Request::SetDisplays {
            modes,
            primary,
            shown_physical,
        } => {
            // Everything checked before anything changes.
            let max = usize::from(limits.virtual_max);
            if modes.is_empty() {
                bail!(UserError::BadArgument);
            }
            if modes.len() > max {
                bail!(UserError::Limit {
                    max,
                    of: Counted::Monitors
                });
            }
            let specs = modes
                .into_iter()
                .map(|m| checked(m, limits))
                .collect::<Result<Vec<MonitorSpec>>>()?;
            let physical = displays.physical_ids();
            let physical_max = usize::from(limits.physical_max);
            let shown: Vec<ScreenId> = shown_physical
                .into_iter()
                .map(ScreenId::from_wire)
                .collect();
            if shown.iter().any(|id| !physical.contains(id)) {
                bail!(UserError::UnknownDisplay);
            }
            if shown.len() > physical_max {
                bail!(UserError::Limit {
                    max: physical_max,
                    of: Counted::Physical
                });
            }
            streams_within(specs.len() + shown.len() + displays.window_count(), limits)?;
            let (index, main) = match primary {
                Some(Primary::Index { index }) if usize::from(index) < specs.len() => {
                    (Some(usize::from(index)), None)
                }
                Some(Primary::Display { id }) if physical.contains(&ScreenId::from_wire(id)) => {
                    (None, Some(ScreenId::from_wire(id)))
                }
                Some(Primary::Index { .. }) => bail!(UserError::BadArgument),
                Some(Primary::Display { .. }) => bail!(UserError::UnknownDisplay),
                None => (None, None),
            };
            if asker.caps.has(caps::headset::PHYSICAL) {
                // Those to hide first, so those to show stay within the limit.
                for show in [false, true] {
                    for id in physical.iter().filter(|id| shown.contains(id) == show) {
                        displays.show_physical(*id, show, physical_max)?;
                    }
                }
            }
            let ids = displays.set_monitors(&specs, index, &mut || store.new_id());
            if let (Ok(_), Some(id)) = (&ids, main) {
                displays.set_primary(id)?;
            }
            displays.force_keyframes();
            keep_screens(store, displays, true);
            displays.sync_wallpapers();
            let ids = ids?;
            info!("headset switched to {} monitor(s)", ids.len());
            Outcome::Ids {
                ids: ids.into_iter().map(ScreenId::wire).collect(),
            }
        }
        Request::SetPrimary { id } => {
            let id = ScreenId::from_wire(id);
            displays.set_primary(id)?;
            info!("headset made {id} the primary display");
            keep_screens(store, displays, false);
            displays.sync_wallpapers();
            Outcome::Done
        }
        Request::ShowPhysical { id, shown } => {
            if !asker.caps.has(caps::headset::PHYSICAL) {
                bail!(UserError::Unsupported);
            }
            let id = ScreenId::from_wire(id);
            if shown && !displays.is_shown_physical(id) {
                streams_within(displays.stream_count() + 1, limits)?;
            }
            displays.show_physical(id, shown, usize::from(limits.physical_max))?;
            // What a display seen for the first time gets, from now on.
            store.change(|s| s.mirror = shown);
            displays.apply(&store.wanted());
            keep_screens(store, displays, false);
            Outcome::Done
        }
        Request::SetEncoding {
            bitrate,
            codec,
            software,
            sharp,
        } => {
            if codec.is_some_and(|c| c != Codec::H264 && !asker.caps.decodes(c)) {
                bail!(UserError::BadArgument);
            }
            let now = store.wanted();
            // Kept as asked: the codec stays chosen when another one encodes.
            let bitrate = bitrate.map_or(now.bitrate, |b| u32::from(b).clamp(5, 100));
            let codec = codec.unwrap_or(now.codec);
            let software = software.unwrap_or(now.software);
            // What the PC would encode with: the codec chosen where every headset with
            // a session decodes it, else H.264 (the choice stays saved). Nothing
            // changes while a display streaming now would be too big for the headset
            // in it.
            let headsets = displays.router.headsets();
            let fits = Displays::codec_for(codec, &headsets);
            let (_, would) = crate::encoder::pick(fits, software).context(UserError::Encoder)?;
            if would != displays.codec()
                && let Some((w, h)) = displays.too_big_for(would, asker.caps)
            {
                bail!(UserError::BadSize { w, h, scale: 100 });
            }
            store.set_encoding(bitrate, sharp.unwrap_or(now.sharp), codec, software);
            // Quality and clearer text apply even when the codec cannot change.
            displays.apply(&store.wanted());
            let result = displays.encode_for(codec, software, &headsets);
            // A monitor whose stream could not start again is gone.
            keep_screens(store, displays, false);
            result?;
            Outcome::Done
        }
        Request::SetSound { to_headset } => {
            store.change(|s| s.audio = to_headset);
            displays.apply(&store.wanted());
            Outcome::Done
        }
        Request::DetachWindow => {
            if !asker.caps.has(caps::headset::WINDOWS) {
                bail!(UserError::Unsupported);
            }
            streams_within(displays.stream_count() + 1, limits)?;
            let id = store.new_id();
            displays.detach_window(id)?;
            Outcome::Added { id: id.wire() }
        }
        Request::WindowHere { id } => {
            displays.window_here(ScreenId::from_wire(id))?;
            Outcome::Done
        }
        Request::Screenshot { id } => {
            let path = displays.screenshot(ScreenId::from_wire(id))?;
            Outcome::Saved {
                path: prostir_proto::state::clip(
                    &home_relative(&path),
                    prostir_proto::request::MAX_PATH,
                ),
            }
        }
        Request::FindMouse => match displays.find_mouse() {
            Some((id, u, v)) => Outcome::Pointer {
                id: id.wire(),
                u,
                v,
            },
            None => bail!(UserError::NotOnDisplays),
        },
    })
}

/// `LIMIT` when `streams` would be more than the headset decodes at once
/// (`Limits.streams_max`, from its `HeadsetCaps.streams_max`).
fn streams_within(streams: usize, limits: &Limits) -> Result<()> {
    let max = usize::from(limits.streams_max);
    if streams > max {
        bail!(UserError::Limit {
            max,
            of: Counted::Streams
        });
    }
    Ok(())
}

/// The monitor `mode` asks for, if the PC makes it: a size within its limits and one
/// of its scales.
fn checked(mode: Mode, limits: &Limits) -> Result<MonitorSpec> {
    let fits = (limits.min.0..=limits.max.0).contains(&mode.w)
        && (limits.min.1..=limits.max.1).contains(&mode.h)
        && limits.scales.contains(&mode.scale);
    let spec = MonitorSpec {
        width: mode.w,
        height: mode.h,
        scale: f64::from(mode.scale) / 100.0,
    };
    if !fits || spec.validate().is_err() {
        bail!(UserError::BadSize {
            w: mode.w,
            h: mode.h,
            scale: mode.scale
        });
    }
    Ok(spec)
}

/// `~/Pictures/…` rather than the full path, for showing.
fn home_relative(path: &std::path::Path) -> String {
    let shown = path.to_string_lossy().into_owned();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        crate::publish::limits(&Default::default(), &HeadsetCaps::default(), Codec::H264)
    }

    /// Only sizes within the PC's limits and only its scales.
    #[test]
    fn sizes_are_checked() {
        let l = limits();
        assert!(checked(Mode::new(2560, 1440), &l).is_ok());
        let larger_text = Mode {
            scale: 150,
            ..Mode::new(2560, 1440)
        };
        assert_eq!(checked(larger_text, &l).unwrap().scale, 1.5);
        for bad in [
            Mode::new(100, 100),
            Mode::new(9000, 1440),
            Mode {
                scale: 110,
                ..Mode::new(1920, 1080)
            },
        ] {
            let e = checked(bad, &l).unwrap_err();
            assert_eq!(errors::code_of(&e), ErrorCode::BAD_SIZE, "{bad:?}");
        }
    }

    /// Each kind of request needs its right; finding the mouse needs none.
    #[test]
    fn rights_per_request() {
        let guest = Access {
            role: prostir_proto::state::role::GUEST,
            perms: perms::INPUT,
        };
        assert!(guest.may(needs(&Request::FindMouse)));
        assert!(guest.may(needs(&Request::WindowHere {
            id: prostir_proto::state::DisplayId(1)
        })));
        assert!(!guest.may(needs(&Request::AddDisplay {
            mode: Mode::new(1920, 1080)
        })));
        assert!(!guest.may(needs(&Request::Screenshot {
            id: prostir_proto::state::DisplayId(1)
        })));
        let changes = Access {
            perms: perms::CHANGES,
            ..guest
        };
        assert!(
            !changes.may(needs(&Request::DetachWindow)),
            "titles are private"
        );
        assert!(Access::MAIN.may(needs(&Request::DetachWindow)));
    }
}
