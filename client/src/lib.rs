//! Quest 3 client: shows each virtual KDE monitor as an OpenXR quad layer.
//!
//! Video frames are decoded by MediaCodec directly into Android surfaces owned
//! by the Quest compositor (XR_KHR_android_surface_swapchain), so pixels never
//! pass through this app's GPU code and text stays as sharp as the compositor
//! can make it.

pub mod background;
pub mod layout;
pub mod settings;
pub mod ui;

#[cfg(target_os = "android")]
mod app;
#[cfg(target_os = "android")]
mod audio;
#[cfg(target_os = "android")]
mod decoder;
#[cfg(target_os = "android")]
mod egl;
#[cfg(target_os = "android")]
mod input;
#[cfg(target_os = "android")]
mod net;
#[cfg(target_os = "android")]
mod overlay;
#[cfg(target_os = "android")]
mod passthrough;
#[cfg(target_os = "android")]
mod xrutil;
