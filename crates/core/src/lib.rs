#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]

pub mod buffer;
pub mod controls;
pub mod format;
pub mod metrics;
pub mod queue;
pub mod transform;

pub mod prelude {
    pub use crate::{
        buffer::{
            BackendFrameMeta, BufferLease, BufferPool, BufferPoolMetrics, BufferPoolStats,
            ExternalBacking, FrameAllocation, FrameLease, FrameLeaseDescriptor, FrameMeta,
            FrameMutability, FramePlaneDescriptor, FramePlaneShape, FrameResidency,
            FrameValidationError, Plane, PlaneLayout, PlaneMut, ResidencyTransition,
            ResidencyTransitionReason, V4l2FrameMeta, VisibleRow, VisibleRowMut, VisibleRows,
            VisibleRowsMut, plane_layout_from_dims, plane_layout_with_stride,
        },
        controls::{Access, ControlId, ControlKind, ControlMeta, ControlMetadata, ControlValue},
        format::{
            BitDepth, Channel, ChromaSubsampling, ColorSpace, FormatInfo, FourCc, FrameLayoutInfo,
            FrameStorageKind, Interval, IntervalStepwise, MediaFormat, PackedChannelOrder,
            PackedPixelSchema, PlaneSchema, Resolution,
        },
        metrics::Metrics,
        queue::{
            BoundedRx, BoundedTx, DEFAULT_QUEUE_CAPACITY, QueueStats, RecvOutcome, RecvWaitOutcome,
            SendOutcome, SendWaitOutcome, bounded, default_bounded, newest,
        },
        transform::{
            FrameTransform, Rotation90, TransformError, TransformPoolConfig,
            TransformResidencyCapabilities, configure_transform_pool,
            packed_transform_residency_capabilities, transform_packed_frame, transform_pool_config,
            transform_pool_stats,
        },
    };

    #[cfg(unix)]
    pub use crate::buffer::{FrameBackingExport, FrameExportError, FrameFdPlane};

    #[cfg(target_os = "linux")]
    pub use crate::buffer::{SharedBufferLease, SharedBufferPool, SharedBufferPoolStats};
}
