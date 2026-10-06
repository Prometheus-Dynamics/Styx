//! Frames and their memory, `no_std` + `alloc`.
//!
//! Frame metadata ([`FrameMeta`] and the backend records), plane layouts and their math
//! ([`PlaneLayout`], [`plane_layout_from_dims`], [`FrameAllocation`], [`FrameValidationError`]),
//! borrowed plane views ([`Plane`], [`VisibleRows`]), frames ([`FrameLease`]) over pooled heap
//! buffers ([`BufferPool`]), over caller-provided memory ([`MemoryRegion`]: a static buffer or a
//! DMA region, read-only or written in place, with [`RegionHooks`] for cache maintenance and
//! giving the buffer back) or over any [`ExternalBacking`], shared views and companions. With `std` on unix: memfd / dma-buf
//! backings, their export and import, the memfd pool ([`SharedBufferPool`], Linux).

mod clock;
mod cpu_access;
#[cfg(all(feature = "std", target_os = "linux"))]
mod dmabuf_sync;
mod frame;
mod hops;
mod layout;
mod meta;
mod plane;
mod pool;
mod views;

pub use cpu_access::CpuAccess;
pub use layout::{plane_layout_from_dims, plane_layout_with_stride};
pub use meta::{
    BackendFrameMeta, CaptureInstant, ClockConversion, ClockSource, FrameHops, FrameLatency,
    FrameMeta, FrameMutability, FrameResidency, FrameTiming, HOP_COUNT, Hop, HopRecord,
    LibcameraFrameMeta, NativeFrameMeta, PlatformClock, ResidencyTransition,
    ResidencyTransitionReason, TimestampClock, UvcFrameMeta, V4l2FrameMeta, set_platform_clock,
};
pub use plane::{
    FrameAllocation, FrameLeaseDescriptor, FramePlaneDescriptor, FrameValidationError, PlaneLayout,
};
pub use views::{
    FramePlaneShape, Plane, PlaneMut, VisibleRow, VisibleRowMut, VisibleRows, VisibleRowsMut,
};

#[cfg(all(feature = "lease-codec", unix))]
pub(crate) use frame::fd_size;
pub use frame::{
    CompanionKind, ExternalBacking, FrameLease, FrameLeaseParts, MemoryRegion, RegionHooks,
    box_downscale_luma, box_downscale_luma_in, shared_backing,
};
#[cfg(all(feature = "std", unix))]
pub use frame::{ExportedKind, FrameBackingExport, FrameExportError, FrameFdPlane};
pub use pool::{BufferLease, BufferPool, BufferPoolMetrics, BufferPoolStats};

#[cfg(all(feature = "std", target_os = "linux"))]
pub use dmabuf_sync::{
    dmabuf_begin_cpu_read, dmabuf_begin_cpu_write, dmabuf_end_cpu_read, dmabuf_end_cpu_write,
};
#[cfg(all(feature = "std", target_os = "linux"))]
pub use pool::{SharedBufferLease, SharedBufferPool, SharedBufferPoolStats};

#[cfg(test)]
mod cpu_access_tests;
#[cfg(test)]
mod tests;
