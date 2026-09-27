#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]

pub mod buffer;
pub mod controls;
pub mod format;
pub mod metrics;
pub mod queue;
pub mod requirements;
pub mod simd;
pub mod transform;

pub mod prelude {
    pub use crate::{
        buffer::{
            BackendFrameMeta, BufferLease, BufferPool, BufferPoolMetrics, BufferPoolStats,
            ClockConversion, ClockSource, CompanionKind, ExternalBacking, FrameAllocation,
            FrameLatency, FrameLease, FrameLeaseDescriptor, FrameMeta, FrameMutability,
            FramePlaneDescriptor, FramePlaneShape, FrameResidency, FrameTiming,
            FrameValidationError, LibcameraFrameMeta, Plane, PlaneLayout, PlaneMut,
            ResidencyTransition, ResidencyTransitionReason, TimestampClock, V4l2FrameMeta,
            VisibleRow, VisibleRowMut, VisibleRows, VisibleRowsMut, box_downscale_luma,
            box_downscale_luma_in, plane_layout_from_dims, plane_layout_with_stride,
        },
        controls::{
            Access, ControlId, ControlKind, ControlMeta, ControlMetadata, ControlRect, ControlValue,
        },
        format::{
            BitDepth, Channel, ChromaSubsampling, ColorSpace, FormatInfo, FourCc, FrameLayoutInfo,
            FrameStorageKind, Interval, IntervalStepwise, MediaFormat, PackedChannelOrder,
            PackedPixelSchema, PlaneSchema, Resolution,
        },
        metrics::Metrics,
        queue::{
            BoundedRx, BoundedTx, DEFAULT_QUEUE_CAPACITY, QueueOverflow, QueueStats, RecvOutcome,
            RecvWaitOutcome, SendOutcome, SendWaitOutcome, bounded, bounded_with, default_bounded,
            newest,
        },
        requirements::{
            FrameRect, FrameRequirements, HardwarePolicy, OutputFormat, PlanOverrides, Priority,
            PyramidRequest, PyramidSource,
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
    pub use crate::buffer::{
        SharedBufferLease, SharedBufferPool, SharedBufferPoolStats, dmabuf_begin_cpu_read,
        dmabuf_end_cpu_read,
    };
}
