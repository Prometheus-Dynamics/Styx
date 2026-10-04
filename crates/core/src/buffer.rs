//! Frames and their memory.
//!
//! `no_std` (always built): frame metadata ([`FrameMeta`] and the backend records), plane
//! layouts and their math ([`PlaneLayout`], [`plane_layout_from_dims`], [`FrameAllocation`],
//! [`FrameValidationError`]) and borrowed plane views ([`Plane`], [`VisibleRows`]). With `std`:
//! frames and pools ([`FrameLease`], [`BufferPool`]), memfd / dma-buf backings.

mod cpu_access;
#[cfg(all(feature = "std", target_os = "linux"))]
mod dmabuf_sync;
#[cfg(feature = "std")]
mod frame;
mod layout;
mod meta;
mod plane;
#[cfg(feature = "std")]
mod pool;
mod views;

pub use cpu_access::CpuAccess;
pub use layout::{plane_layout_from_dims, plane_layout_with_stride};
pub use meta::{
    BackendFrameMeta, CaptureInstant, ClockConversion, ClockSource, FrameLatency, FrameMeta,
    FrameMutability, FrameResidency, FrameTiming, LibcameraFrameMeta, NativeFrameMeta,
    ResidencyTransition, ResidencyTransitionReason, TimestampClock, UvcFrameMeta, V4l2FrameMeta,
};
pub use plane::{
    FrameAllocation, FrameLeaseDescriptor, FramePlaneDescriptor, FrameValidationError, PlaneLayout,
};
pub use views::{
    FramePlaneShape, Plane, PlaneMut, VisibleRow, VisibleRowMut, VisibleRows, VisibleRowsMut,
};

#[cfg(feature = "std")]
pub use frame::{
    CompanionKind, ExternalBacking, FrameLease, box_downscale_luma, box_downscale_luma_in,
};
#[cfg(all(feature = "std", unix))]
pub use frame::{FrameBackingExport, FrameExportError, FrameFdPlane};
#[cfg(feature = "std")]
pub use pool::{BufferLease, BufferPool, BufferPoolMetrics, BufferPoolStats};

#[cfg(all(feature = "std", target_os = "linux"))]
pub use dmabuf_sync::{dmabuf_begin_cpu_read, dmabuf_end_cpu_read};
#[cfg(all(feature = "std", target_os = "linux"))]
pub use pool::{SharedBufferLease, SharedBufferPool, SharedBufferPoolStats};

#[cfg(test)]
mod cpu_access_tests;
#[cfg(test)]
mod tests;
