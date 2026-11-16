//! GStreamer source for Styx frames: `styxsrc`, a GstPushSrc that reads frames from a Styx
//! producer over the shared ring ([`styx_socket`]) and hands each frame to GStreamer as a buffer
//! whose memory is the frame's own memory. No frame is copied.
//!
//! - [`Source`] is the frame half, and needs no GStreamer: it connects to the producer's socket,
//!   holds each frame it hands on, and reconnects with backoff (250 ms doubling to 5 s) when the
//!   producer goes away or comes back, reporting a [`LinkStatus`]. The reader holds a frame until
//!   the consumer drops it, so the producer cannot reuse that buffer meanwhile.
//! - The element (`styxsrc`) is Rust on gstreamer-rs, built with the `gstreamer` feature. It is a
//!   `PushSrc` subclass: each frame's memory wraps the frame's own bytes (read-only), or is a
//!   dma-buf over its descriptor, and the frame goes back to the producer when GStreamer frees
//!   that memory. It needs GStreamer 1.x installed (base, video and allocators) when built.
//! - Plugin: with the feature on, the crate also builds as a shared object GStreamer loads by
//!   `GST_PLUGIN_PATH=<dir> gst-launch-1.0 styxsrc socket=<path> ! ...`.
//!
//! Frames the element cannot name (H.264, say, which GStreamer has no caps name for here; or memory
//! that is neither mapped nor a descriptor) are dropped and counted. MJPG frames are `image/jpeg`.
//! Linux only: the ring is a Linux layout.
//!
//! `unsafe` appears only at the GStreamer boundary (`element`), each block with a `SAFETY` note.

#![cfg(target_os = "linux")]

mod link;
mod source;

#[cfg(feature = "gstreamer")]
mod element;

#[cfg(feature = "gstreamer")]
use gstreamer as gst;

pub use link::{Backoff, LinkState, LinkStatus};
pub use source::{Delivered, Source, SourceStats, gst_format};

/// Whether the GStreamer element was built into this crate: the `gstreamer` feature is on.
pub const fn element_built() -> bool {
    cfg!(feature = "gstreamer")
}

#[cfg(feature = "gstreamer")]
fn plugin_init(plugin: &gst::Plugin) -> Result<(), gst::glib::BoolError> {
    element::register(plugin)
}

// The plugin's entry points (`gst_plugin_styx_get_desc` and friends) for GStreamer to load it by.
#[cfg(feature = "gstreamer")]
gst::plugin_define!(
    styx,
    "Styx frames from the shared ring",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT",
    "styx-gst",
    "styx",
    "https://github.com/Prometheus-Dynamics/Styx"
);
