//! V4L2 capture nodes as Styx cameras: any capture node (UVC webcams through uvcvideo, other
//! boards' capture drivers) on Lemnos's `lemnos-v4l2` library, used as a plain library through
//! `styx-lemnos`'s re-export. No lemnosd is needed. Styx carries no V4L2 bindings of its own.
//!
//! - [`probe`] lists the video nodes from sysfs and opens none of them. [`probe_node`],
//!   [`probe_card`] and [`probe_input`] fully probe one named node (formats, frame sizes and
//!   intervals, and the controls Styx maps to [`styx_controls::Control`]), opening that node once.
//!   A virtual node (another program's loopback camera) is opened only when a caller names it.
//! - [`Stream::start`] negotiates a format and streams it. Frames are [`styx_frame::FrameLease`]s
//!   with [`styx_controls::FrameMetadata`]: DMABUF-exported buffers are handed on without a copy,
//!   MMAP buffers are copied (counted in [`copy_counters`]). Timestamps are `CLOCK_MONOTONIC`
//!   nanoseconds: the driver's when it flags them monotonic, otherwise the arrival time.
//! - A stream that loses its device retries with backoff capped at five seconds, reports that in
//!   [`Status`], and stops only on [`Stream::stop`] or when a configured policy gives up.

#![cfg(target_os = "linux")]

mod controls;
mod error;
mod layout;
mod probe;
mod stream;

pub use error::Error;
pub use probe::{
    Device, DeviceMenuItem, Format, Intervals, Listing, Node, Opener, Prober, Queue, RawControl,
    Size, SizeRange, Sizes, System, probe, probe_card, probe_input, probe_node,
};
pub use stream::{Delivery, State, Status, Stream, StreamConfig};

use styx_frame::CopyCounter;

/// Bytes copied from MMAP buffers into frames. Zero when every frame is DMABUF-exported.
static MMAP_COPY: CopyCounter = CopyCounter::new("styx-v4l2: mmap capture copy");
static COUNTERS: [&CopyCounter; 1] = [&MMAP_COPY];

/// The copy counters of this backend, for the copy accounting of a pipeline.
pub fn copy_counters() -> &'static [&'static CopyCounter] {
    &COUNTERS
}

pub(crate) fn record_mmap_copy(bytes: usize) {
    MMAP_COPY.record(bytes);
}
