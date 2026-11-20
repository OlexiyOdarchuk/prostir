//! One graphics device per card for the encoders (and captures) that run at once.
//!
//! Each GStreamer pipeline opens a device of its own unless it is given one: a VA
//! display (a render node of the card and the driver loaded on it), a CUDA context, a
//! Direct3D 11 device. With a pipeline per output, N monitors would open N of them. On
//! Windows, six processes each with its own CUDA context fell to about 6 frames a second
//! each, while the same six encoders in one process on one device ran in full
//! (`plans/windows-prototype.md`, section 8). So every pipeline asks here first.
//!
//! # VA-API keeps a display per pipeline
//!
//! A VA display is not a context like CUDA's: each is a file of the render node with
//! the driver loaded on it, and the driver serves several of them side by side. Shared,
//! five monitors held one render node file instead of five, but the encoders were no
//! faster: on an Intel Arc (iHD 26.3.5), 4 streams of 1920x1080 at 60 frames a second,
//! GPU buffers from a producer with its own display as KWin's are, encoder time p50
//! 8.57 / 4.81 ms with a display each against 8.47 / 5.07 ms shared, p99 11.54 / 6.18
//! against 12.28 / 7.40 ms (two quiet pairs of 15 s each; the PC was busy otherwise
//! and other runs varied more between themselves than between the two ways). One lock
//! of one display for every encoder costs at the tail, and saves only memory. So VA
//! displays are not shared ([`SHARE_VA`]), and `encoder::live::va_display_sharing`
//! measures it again by hand. CUDA contexts and Direct3D 11 devices are.
//!
//! GStreamer's elements ask for a device with a `need-context` message and say the one
//! they opened with `have-context`. [`GpuDevices::share_with`] answers the first from
//! what it keeps, by context type and card, and keeps the first device each card opens
//! from the second. The card an element runs on is its own property: `device-path` (VA:
//! the render node), `cuda-device-id` (CUDA), `adapter-luid` (Direct3D 11, the LUID a
//! capture of the same card reports in [`crate::platform::Memory::D3D11`]).
//!
//! A device is kept only while a pipeline holds it: each pipeline gets a [`Lease`] of
//! the devices it took, and the last lease of a device let go closes it. So a card no
//! stream encodes on any more (a PRIME graphics card, say, once its outputs moved to
//! the CPU) is free to sleep, and an idle PC holds none.
//!
//! # The bus's one sync handler
//!
//! GStreamer keeps one synchronous handler per bus, and setting another replaces it:
//! the handler set here is the pipeline's only one, and no pipeline of Prostir sets
//! another. One that needs its own must call this handler from it (chain them), or it
//! loses the shared devices without a word.
//!
//! # Pipelines started at the same moment
//!
//! Pipelines start one after another on one thread now. A worker that starts several
//! at once on threads of their own (the desktop's worker of a later step) can have two
//! of them open a device of the same card before either says so: the first one said is
//! kept and shared, the other pipeline keeps its own for its life (one device more,
//! nothing wrong). Starting the first pipeline of a card before the others avoids it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, Weak};

use gstreamer as gst;
use gstreamer::prelude::*;
use log::info;

/// The context types shared, with the property that names an element's card.
const SHARED: &[(&str, &str)] = &[
    (VA, "device-path"),
    ("gst.cuda.context", "cuda-device-id"),
    ("gst.d3d11.device.handle", "adapter-luid"),
];

/// The VA display's context type.
const VA: &str = "gst.va.display.handle";

/// Whether VA displays are shared too (see the top of this file).
const SHARE_VA: bool = false;

/// A device: its context type and card.
type Key = (String, String);

/// The devices kept, each with how many pipelines hold it.
type Devices = BTreeMap<Key, (gst::Context, usize)>;

/// The graphics devices captures and encoders share: one per card and kind, while a
/// pipeline holds it. Cheap to clone: every clone shares the same devices.
#[derive(Debug, Clone)]
pub struct GpuDevices {
    devices: Arc<Mutex<Devices>>,
    /// VA displays are shared too.
    va: bool,
}

impl Default for GpuDevices {
    fn default() -> Self {
        Self {
            devices: Arc::default(),
            va: SHARE_VA,
        }
    }
}

/// The devices one pipeline holds; dropped after the pipeline stopped, it lets them
/// go, and closes each that no other pipeline holds.
#[derive(Debug, Default)]
pub struct Lease {
    devices: Weak<Mutex<Devices>>,
    held: Arc<Mutex<BTreeSet<Key>>>,
}

impl GpuDevices {
    /// `pipeline` takes the devices kept here, and the first it opens of a kind and card
    /// is kept for the others, while the lease returned lives. Before the pipeline
    /// leaves the `NULL` state. It sets the bus's sync handler (see the top of this
    /// file).
    pub fn share_with(&self, pipeline: &gst::Pipeline) -> Lease {
        let lease = self.lease();
        if let Some(bus) = pipeline.bus() {
            let devices = self.clone();
            let held = lease.held.clone();
            bus.set_sync_handler(move |_, msg| {
                devices.on_message(&held, msg);
                gst::BusSyncReply::Pass
            });
        }
        lease
    }

    /// Devices that share VA displays too, for measuring both ways.
    #[cfg(test)]
    pub fn sharing_va() -> Self {
        Self {
            va: true,
            ..Self::default()
        }
    }

    /// Whether context type `kind` is shared here.
    fn shares(&self, kind: &str) -> bool {
        SHARED.iter().any(|(k, _)| *k == kind) && (self.va || kind != VA)
    }

    fn lease(&self) -> Lease {
        Lease {
            devices: Arc::downgrade(&self.devices),
            held: Arc::default(),
        }
    }

    /// Answers a pipeline's need for a device, or keeps the one it opened, for the
    /// pipeline whose lease holds `held`.
    fn on_message(&self, held: &Mutex<BTreeSet<Key>>, msg: &gst::Message) {
        let Some(element) = msg.src().and_then(|s| s.downcast_ref::<gst::Element>()) else {
            return;
        };
        match msg.view() {
            gst::MessageView::NeedContext(m) => {
                let kind = m.context_type();
                if !self.shares(kind) {
                    return;
                }
                let key = (kind.to_owned(), card(element, kind));
                let context = {
                    let mut devices = self.devices.lock().unwrap();
                    let Some((context, users)) = devices.get_mut(&key) else {
                        return;
                    };
                    if held.lock().unwrap().insert(key) {
                        *users += 1;
                    }
                    context.clone()
                };
                element.set_context(&context);
            }
            gst::MessageView::HaveContext(m) => {
                let context = m.context();
                let kind = context.context_type();
                if !self.shares(kind) {
                    return;
                }
                let key = (kind.to_owned(), card(element, kind));
                let mut devices = self.devices.lock().unwrap();
                match devices.get_mut(&key) {
                    None => {
                        info!("sharing {} {} with every encoder", key.0, key.1);
                        held.lock().unwrap().insert(key.clone());
                        devices.insert(key, (context, 1));
                    }
                    // Said again of the one kept (a bin passing it on): held once.
                    Some((kept, users)) if kept.as_ptr() == context.as_ptr() => {
                        if held.lock().unwrap().insert(key) {
                            *users += 1;
                        }
                    }
                    // Opened at the same moment as the one kept: this pipeline keeps
                    // its own (see the top of this file).
                    Some(_) => {}
                }
            }
            _ => {}
        }
    }

    /// The devices kept now, as `type card`.
    #[cfg(test)]
    fn kept(&self) -> Vec<String> {
        self.devices
            .lock()
            .unwrap()
            .keys()
            .map(|(kind, card)| format!("{kind} {card}"))
            .collect()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(devices) = self.devices.upgrade() else {
            return;
        };
        let mut devices = devices.lock().unwrap();
        for key in std::mem::take(&mut *self.held.lock().unwrap()) {
            if let Some((_, users)) = devices.get_mut(&key) {
                *users -= 1;
                if *users == 0 {
                    info!("closing {} {}: no encoder uses it now", key.0, key.1);
                    devices.remove(&key);
                }
            }
        }
    }
}

/// The card element `element` runs on, by the property context type `kind` names it
/// with; empty when it has none (the default card).
fn card(element: &gst::Element, kind: &str) -> String {
    let Some(property) = SHARED.iter().find(|(k, _)| *k == kind).map(|(_, p)| *p) else {
        return String::new();
    };
    if element.find_property(property).is_none() {
        return String::new();
    }
    let value = element.property_value(property);
    value
        .get::<String>()
        .ok()
        .or_else(|| value.get::<u32>().ok().map(|v| v.to_string()))
        .or_else(|| value.get::<i64>().ok().map(|v| v.to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device another pipeline said it opened: kept while a lease holds it, given to
    /// the next pipeline that asks (held by its lease too), closed with the last lease.
    #[test]
    fn a_device_lives_as_long_as_a_pipeline_holds_it() {
        gst::init().unwrap();
        let gpu = GpuDevices::default();
        let element = gst::ElementFactory::make("identity").build().unwrap();
        let context = gst::Context::new("gst.cuda.context", true);
        let have = gst::message::HaveContext::builder(context.clone())
            .src(&element)
            .build();
        let first = gpu.lease();
        gpu.on_message(&first.held, &have);
        assert_eq!(gpu.kept(), vec!["gst.cuda.context ".to_owned()]);
        let need = gst::message::NeedContext::builder("gst.cuda.context")
            .src(&element)
            .build();
        let second = gpu.lease();
        gpu.on_message(&second.held, &need);
        // Asked twice by the same pipeline (it started over): held once.
        gpu.on_message(&second.held, &need);
        drop(first);
        assert_eq!(gpu.kept().len(), 1, "the second pipeline still holds it");
        // Another device opened at the same moment is not kept over it.
        let other = gst::Context::new("gst.cuda.context", true);
        let late = gst::message::HaveContext::builder(other)
            .src(&element)
            .build();
        let third = gpu.lease();
        gpu.on_message(&third.held, &late);
        drop(third);
        assert_eq!(gpu.kept().len(), 1);
        drop(second);
        assert!(gpu.kept().is_empty(), "closed with its last pipeline");
        // Nothing kept: nothing to give.
        let fourth = gpu.lease();
        gpu.on_message(&fourth.held, &need);
        assert!(fourth.held.lock().unwrap().is_empty());
    }
}
