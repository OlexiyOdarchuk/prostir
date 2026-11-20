//! The headset's requests: each changes the PC's state and gets exactly one outcome.
//! The parts it changed go out before the reply (see `publish`).

use log::info;
use prostir_proto::request::{Outcome, Primary, Request};
use prostir_proto::state::{ErrorCode, HeadsetCaps, Mode, caps};

use crate::displays::Displays;
use crate::keep_screens;
use crate::platform::MonitorSpec;
use crate::screen::ScreenId;
use crate::store::Store;

/// Carries out `request` for a headset that can do what `headset` says.
pub fn handle(
    request: Request,
    displays: &mut Displays,
    store: &mut Store,
    headset: &HeadsetCaps,
) -> Outcome {
    let failed = |e: anyhow::Error| Outcome::failed(ErrorCode::OTHER, &format!("{e:#}"));
    match request {
        Request::AddDisplay { mode } => {
            let id = store.new_id();
            match displays.add(id, spec(mode)) {
                Ok(()) => {
                    info!("headset added {id}");
                    displays.force_keyframe(id);
                    keep_screens(store, displays, true);
                    displays.sync_wallpapers();
                    Outcome::Added { id: id.wire() }
                }
                Err(e) => failed(e),
            }
        }
        Request::RemoveDisplay { id } => {
            let id = ScreenId::from_wire(id);
            let monitor = displays.is_monitor(id);
            match displays.remove(id) {
                Ok(()) => {
                    info!("headset removed {id}");
                    keep_screens(store, displays, monitor);
                    Outcome::Done
                }
                Err(e) => failed(e),
            }
        }
        Request::SetDisplays {
            modes,
            primary,
            shown_physical,
        } => {
            let specs: Vec<MonitorSpec> = modes.into_iter().map(spec).collect();
            let primary = match primary {
                Some(Primary::Index { index }) => Some(usize::from(index)),
                _ => None,
            };
            if headset.has(caps::headset::PHYSICAL) {
                for id in displays.physical_ids() {
                    let show = shown_physical.contains(&id.wire());
                    displays.show_physical(id, show);
                }
            }
            let result = displays.set_monitors(&specs, primary, &mut || store.new_id());
            displays.force_keyframes();
            keep_screens(store, displays, true);
            displays.sync_wallpapers();
            match result {
                Ok(ids) => {
                    info!("headset switched to {} monitor(s)", ids.len());
                    Outcome::Ids {
                        ids: ids.into_iter().map(ScreenId::wire).collect(),
                    }
                }
                Err(e) => failed(e),
            }
        }
        Request::SetPrimary { id } => {
            let id = ScreenId::from_wire(id);
            if !displays.is_monitor(id) {
                return Outcome::failed(ErrorCode::OTHER, &format!("no {id}"));
            }
            info!("headset made {id} the primary monitor");
            displays.set_primary(id);
            keep_screens(store, displays, false);
            displays.sync_wallpapers();
            Outcome::Done
        }
        Request::ShowPhysical { id, shown } => {
            let id = ScreenId::from_wire(id);
            if !displays.show_physical(id, shown) {
                return Outcome::failed(ErrorCode::OTHER, &format!("no {id}"));
            }
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
            let now = store.wanted();
            // Kept as asked: the codec stays chosen when another one encodes.
            let bitrate = bitrate.map_or(now.bitrate, |b| u32::from(b).clamp(5, 100));
            let codec = codec.unwrap_or(now.codec);
            let software = software.unwrap_or(now.software);
            store.set_encoding(bitrate, sharp.unwrap_or(now.sharp), codec, software);
            // Quality and clearer text apply even when the codec cannot change.
            displays.apply(&store.wanted());
            let result = displays.encode_with(codec, software);
            displays.fit_codec(headset);
            // A monitor whose stream could not start again is gone.
            keep_screens(store, displays, false);
            match result {
                Ok(()) => Outcome::Done,
                Err(e) => failed(e),
            }
        }
        Request::SetSound { to_headset } => {
            store.change(|s| s.audio = to_headset);
            displays.apply(&store.wanted());
            Outcome::Done
        }
        Request::DetachWindow => {
            let id = store.new_id();
            match displays.detach_window(id) {
                Ok(()) => Outcome::Added { id: id.wire() },
                Err(e) => failed(e),
            }
        }
        Request::WindowHere { id } => match displays.window_here(ScreenId::from_wire(id)) {
            Ok(()) => Outcome::Done,
            Err(e) => failed(e),
        },
        Request::Screenshot { id } => match displays.screenshot(ScreenId::from_wire(id)) {
            Ok(path) => Outcome::Saved {
                path: prostir_proto::state::clip(
                    &home_relative(&path),
                    prostir_proto::request::MAX_PATH,
                ),
            },
            Err(e) => failed(e),
        },
        Request::FindMouse => match displays.find_mouse() {
            Some((id, u, v)) => Outcome::Pointer {
                id: id.wire(),
                u,
                v,
            },
            None => Outcome::failed(ErrorCode::OTHER, "the mouse is on none of the displays"),
        },
    }
}

/// The monitor a mode asks for: its size, and its scale (percent) as KWin's.
fn spec(mode: Mode) -> MonitorSpec {
    MonitorSpec {
        width: mode.w,
        height: mode.h,
        scale: f64::from(mode.scale.max(1)) / 100.0,
    }
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
