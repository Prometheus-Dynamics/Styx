#[cfg(target_os = "linux")]
mod dmabuf_sync;
mod frame;
mod meta;
mod pool;

pub use frame::{
    CompanionKind, ExternalBacking, FrameAllocation, FrameLease, FrameLeaseDescriptor,
    FramePlaneDescriptor, FramePlaneShape, FrameValidationError, Plane, PlaneLayout, PlaneMut,
    VisibleRow, VisibleRowMut, VisibleRows, VisibleRowsMut, box_downscale_luma,
    box_downscale_luma_in, plane_layout_from_dims, plane_layout_with_stride,
};

#[cfg(unix)]
pub use frame::{FrameBackingExport, FrameExportError, FrameFdPlane};
pub use meta::{
    BackendFrameMeta, ClockConversion, ClockSource, FrameLatency, FrameMeta, FrameMutability,
    FrameResidency, FrameTiming, LibcameraFrameMeta, NativeFrameMeta, ResidencyTransition,
    ResidencyTransitionReason, TimestampClock, V4l2FrameMeta,
};
pub use pool::{BufferLease, BufferPool, BufferPoolMetrics, BufferPoolStats};

#[cfg(target_os = "linux")]
pub use dmabuf_sync::{dmabuf_begin_cpu_read, dmabuf_end_cpu_read};
#[cfg(target_os = "linux")]
pub use pool::{SharedBufferLease, SharedBufferPool, SharedBufferPoolStats};

#[cfg(test)]
mod tests;
