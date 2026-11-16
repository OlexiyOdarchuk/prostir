//! XR_FB_passthrough driven through the raw entry points.
//!
//! Hiding only the passthrough layer keeps the cameras and the reconstruction
//! pipeline running and the battery draining, so "off" pauses the feature itself.
//! (`openxr::Passthrough::start` calls xrPassthroughPauseFB, hence the raw calls.)

use std::ptr;

use anyhow::{Context, Result, bail};
use openxr as xr;
use openxr::sys::Handle as _;

pub struct Passthrough {
    fp: xr::raw::PassthroughFB,
    feature: xr::sys::PassthroughFB,
    layer: xr::sys::PassthroughLayerFB,
    running: bool,
}

fn check(result: xr::sys::Result, what: &str) -> Result<()> {
    if result.into_raw() < 0 {
        bail!("{what} failed: {result:?}");
    }
    Ok(())
}

impl Passthrough {
    pub fn new<G: xr::Graphics>(session: &xr::Session<G>, running: bool) -> Result<Self> {
        let fp = *session
            .instance()
            .exts()
            .fb_passthrough
            .as_ref()
            .context("XR_FB_passthrough is not enabled")?;
        let flags = if running {
            xr::PassthroughFlagsFB::IS_RUNNING_AT_CREATION
        } else {
            xr::PassthroughFlagsFB::EMPTY
        };

        let info = xr::sys::PassthroughCreateInfoFB {
            ty: xr::sys::PassthroughCreateInfoFB::TYPE,
            next: ptr::null(),
            flags,
        };
        let mut feature = xr::sys::PassthroughFB::NULL;
        // SAFETY: valid session, create info and out-pointer.
        check(
            unsafe { (fp.create_passthrough)(session.as_raw(), &info, &mut feature) },
            "xrCreatePassthroughFB",
        )?;

        let info = xr::sys::PassthroughLayerCreateInfoFB {
            ty: xr::sys::PassthroughLayerCreateInfoFB::TYPE,
            next: ptr::null(),
            passthrough: feature,
            flags,
            purpose: xr::PassthroughLayerPurposeFB::RECONSTRUCTION,
        };
        let mut layer = xr::sys::PassthroughLayerFB::NULL;
        // SAFETY: `feature` was just created for this session.
        let result = unsafe { (fp.create_passthrough_layer)(session.as_raw(), &info, &mut layer) };
        if let Err(e) = check(result, "xrCreatePassthroughLayerFB") {
            // SAFETY: destroying the handle created above, never used again.
            unsafe { (fp.destroy_passthrough)(feature) };
            return Err(e);
        }
        Ok(Self {
            fp,
            feature,
            layer,
            running,
        })
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn layer(&self) -> xr::sys::PassthroughLayerFB {
        self.layer
    }

    /// Starts or pauses the cameras together with the layer.
    pub fn set_running(&mut self, on: bool) -> Result<()> {
        if on == self.running {
            return Ok(());
        }
        // SAFETY: both handles are alive for the lifetime of `self`.
        unsafe {
            if on {
                check(
                    (self.fp.passthrough_start)(self.feature),
                    "xrPassthroughStartFB",
                )?;
                check(
                    (self.fp.passthrough_layer_resume)(self.layer),
                    "xrPassthroughLayerResumeFB",
                )?;
            } else {
                check(
                    (self.fp.passthrough_layer_pause)(self.layer),
                    "xrPassthroughLayerPauseFB",
                )?;
                check(
                    (self.fp.passthrough_pause)(self.feature),
                    "xrPassthroughPauseFB",
                )?;
            }
        }
        self.running = on;
        Ok(())
    }
}

impl Drop for Passthrough {
    fn drop(&mut self) {
        // SAFETY: handles are owned by `self` and destroyed exactly once, layer first.
        unsafe {
            (self.fp.destroy_passthrough_layer)(self.layer);
            (self.fp.destroy_passthrough)(self.feature);
        }
    }
}
