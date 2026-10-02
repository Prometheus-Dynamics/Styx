//! GStreamer plugin for Styx cameras.
//!
//! - `styxsrc`: a live source. Caps negotiation becomes Styx [`FrameRequirements`]
//!   (format, size, rate) and the Styx planner picks the capture mode and route (direct, decode,
//!   convert, encode). Camera buffers are passed on without copying, as `GstDmaBufMemory` when
//!   `video/x-raw(memory:DMABuf)` is negotiated (or `export-dmabuf` is set).
//! - `styxdeviceprovider`: lists Styx cameras in `GstDeviceMonitor` (`gst-device-monitor-1.0`).
//!
//! [`FrameRequirements`]: styx::prelude::FrameRequirements

use gst::glib;

pub mod buffer;
pub mod caps;
mod provider;
pub mod source;
mod styxsrc;

pub use provider::StyxDeviceProvider;
pub use styxsrc::StyxSrc;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    styxsrc::register(plugin)?;
    provider::register(plugin)?;
    Ok(())
}

/// Register the elements with GStreamer when linking this crate statically (tests, apps).
pub fn register_static() -> Result<(), glib::BoolError> {
    plugin_register_static()
}

gst::plugin_define!(
    styx,
    "Styx camera source and device provider",
    plugin_init,
    "2.0.0",
    "MIT/X11",
    "gst-styx",
    "styx",
    "https://github.com/Prometheus-Dynamics/Styx"
);
