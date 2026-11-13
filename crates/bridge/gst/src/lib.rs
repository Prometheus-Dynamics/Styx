//! GStreamer source for Styx frames: `styxsrc`, a GstPushSrc that reads frames from a Styx
//! producer over the shared ring ([`styx_socket`]) and hands each frame to GStreamer as a buffer
//! whose memory is the frame's own memory. No frame is copied.
//!
//! - [`Source`] is the Rust half, and needs no GStreamer: it connects to the producer's socket,
//!   holds each frame it hands on, and reconnects with backoff (250 ms doubling to 5 s) when the
//!   producer goes away or comes back, reporting a [`LinkStatus`]. The reader holds a frame until
//!   the consumer drops it, so the producer cannot reuse that buffer meanwhile.
//! - The element is C (`c_api/styx_gst.c`), built by `build.rs` when the `gstreamer` feature is on
//!   and pkg-config finds GStreamer 1.x (base and video; allocators for dma-buf memory). The
//!   element uses GStreamer's own object and buffer API, which is C-only, so the C side holds no
//!   frame logic: it calls back into [`Source`] through a table of functions.
//! - Plugin: the crate also builds as a shared object exporting `gst_plugin_desc`, so
//!   `GST_PLUGIN_PATH=<dir> gst-launch-1.0 styxsrc socket=<path> ! ...` loads the element.
//!
//! Frames the element cannot name (H.264, say, which GStreamer has no caps name for here; or memory
//! that is neither mapped nor a descriptor) are dropped and counted. MJPG frames are `image/jpeg`.
//! Linux only: the ring is a Linux layout.
//!
//! `unsafe` appears only at the GStreamer boundary (`plugin`), each block with a `SAFETY` note.

#![cfg(target_os = "linux")]

mod link;
mod source;

#[cfg(has_gstreamer)]
mod plugin;

pub use link::{Backoff, LinkState, LinkStatus};
pub use source::{Delivered, Source, SourceStats, gst_format};

/// Whether the GStreamer element was built into this crate (`gstreamer` feature, and the
/// libraries were found when the crate was built).
pub const fn element_built() -> bool {
    cfg!(has_gstreamer)
}
