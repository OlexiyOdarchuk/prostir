//! One graphics device per card for every capture and encoder.
//!
//! Each GStreamer pipeline opens a device of its own unless it is given one: a VA
//! display (a render node of the card and the driver loaded on it), a CUDA context, a
//! Direct3D 11 device. With a pipeline per output, N monitors would open N of them. On
//! Windows, six processes each with its own CUDA context fell to about 6 frames a second
//! each, while the same six encoders in one process on one device ran in full
//! (`plans/windows-prototype.md`, section 8). So every pipeline asks here first.
//!
//! GStreamer's elements ask for a device with a `need-context` message and say the one
//! they opened with `have-context`. [`GpuDevices::share_with`] answers the first from
//! what it keeps, by context type and card, and keeps the first device each card opens
//! from the second. The card an element runs on is its own property: `device-path` (VA:
//! the render node), `cuda-device-id` (CUDA), `adapter-luid` (Direct3D 11, the LUID a
//! capture of the same card reports in [`crate::platform::Memory::D3D11`]).
//!
//! The devices live as long as the desktop's connection (`Displays` holds them), so an
//! idle PC holds none.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use gstreamer as gst;
use gstreamer::prelude::*;
use log::info;

/// The context types shared, with the property that names an element's card.
const SHARED: &[(&str, &str)] = &[
    ("gst.va.display.handle", "device-path"),
    ("gst.cuda.context", "cuda-device-id"),
    ("gst.d3d11.device.handle", "adapter-luid"),
];

/// The graphics devices captures and encoders share: one per card and kind, for the
/// life of a desktop. Cheap to clone: every clone shares the same devices.
#[derive(Debug, Clone, Default)]
pub struct GpuDevices {
    devices: Arc<Mutex<Devices>>,
}

/// Contexts by type and card.
type Devices = BTreeMap<(String, String), gst::Context>;

impl GpuDevices {
    /// `pipeline` takes the devices kept here, and the first it opens of a kind and card
    /// is kept for the others. Before the pipeline leaves the `NULL` state.
    pub fn share_with(&self, pipeline: &gst::Pipeline) {
        let Some(bus) = pipeline.bus() else {
            return;
        };
        let devices = Arc::downgrade(&self.devices);
        bus.set_sync_handler(move |_, msg| {
            on_message(&devices, msg);
            gst::BusSyncReply::Pass
        });
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

fn on_message(devices: &Weak<Mutex<Devices>>, msg: &gst::Message) {
    let Some(element) = msg.src().and_then(|s| s.downcast_ref::<gst::Element>()) else {
        return;
    };
    match msg.view() {
        gst::MessageView::NeedContext(m) => {
            let kind = m.context_type();
            if !SHARED.iter().any(|(k, _)| *k == kind) {
                return;
            }
            let Some(devices) = devices.upgrade() else {
                return;
            };
            let key = (kind.to_owned(), card(element, kind));
            let context = devices.lock().unwrap().get(&key).cloned();
            if let Some(context) = context {
                element.set_context(&context);
            }
        }
        gst::MessageView::HaveContext(m) => {
            let context = m.context();
            let kind = context.context_type();
            if !SHARED.iter().any(|(k, _)| *k == kind) {
                return;
            }
            let Some(devices) = devices.upgrade() else {
                return;
            };
            let key = (kind.to_owned(), card(element, kind));
            if let std::collections::btree_map::Entry::Vacant(free) =
                devices.lock().unwrap().entry(key)
            {
                info!(
                    "sharing {} {} with every encoder",
                    free.key().0,
                    free.key().1
                );
                free.insert(context);
            }
        }
        _ => {}
    }
}
