#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod buffer;
pub mod controls;
pub mod format;
mod math;
#[cfg(feature = "std")]
pub mod metrics;
#[cfg(feature = "std")]
pub mod queue;
pub mod requirements;
pub mod simd;
#[cfg(feature = "std")]
pub mod transform;

pub mod prelude {
    pub use crate::{
        buffer::{
            BackendFrameMeta, CaptureInstant, ClockConversion, ClockSource, FrameAllocation,
            FrameLatency, FrameLeaseDescriptor, FrameMeta, FrameMutability, FramePlaneDescriptor,
            FramePlaneShape, FrameResidency, FrameTiming, FrameValidationError, LibcameraFrameMeta,
            NativeFrameMeta, Plane, PlaneLayout, PlaneMut, ResidencyTransition,
            ResidencyTransitionReason, TimestampClock, UvcFrameMeta, V4l2FrameMeta, VisibleRow,
            VisibleRowMut, VisibleRows, VisibleRowsMut, plane_layout_from_dims,
            plane_layout_with_stride,
        },
        controls::{
            Access, ControlId, ControlKind, ControlMeta, ControlMetadata, ControlRect, ControlValue,
        },
        format::{
            BitDepth, Channel, ChromaSubsampling, ColorSpace, FormatInfo, FourCc, FrameLayoutInfo,
            FrameStorageKind, Interval, IntervalStepwise, MediaFormat, PackedChannelOrder,
            PackedPixelSchema, PlaneSchema, Resolution,
        },
        requirements::{FrameRect, OutputFormat, PyramidRequest, PyramidSource},
    };

    // The previous frame request, kept for one release (see `requirements`).
    #[allow(deprecated)]
    pub use crate::requirements::{FrameRequirements, HardwarePolicy, PlanOverrides, Priority};

    #[cfg(feature = "std")]
    pub use crate::{
        buffer::{
            BufferLease, BufferPool, BufferPoolMetrics, BufferPoolStats, CompanionKind,
            ExternalBacking, FrameLease, box_downscale_luma, box_downscale_luma_in,
        },
        metrics::Metrics,
        queue::{
            BoundedRx, BoundedTx, DEFAULT_QUEUE_CAPACITY, QueueOverflow, QueueStats, RecvOutcome,
            RecvWaitOutcome, SendOutcome, SendWaitOutcome, bounded, bounded_with, default_bounded,
            newest,
        },
        transform::{
            FrameTransform, Rotation90, TransformError, TransformPoolConfig,
            TransformResidencyCapabilities, configure_transform_pool,
            packed_transform_residency_capabilities, transform_packed_frame, transform_pool_config,
            transform_pool_stats,
        },
    };

    #[cfg(all(feature = "std", unix))]
    pub use crate::buffer::{FrameBackingExport, FrameExportError, FrameFdPlane};

    #[cfg(all(feature = "std", target_os = "linux"))]
    pub use crate::buffer::{
        SharedBufferLease, SharedBufferPool, SharedBufferPoolStats, dmabuf_begin_cpu_read,
        dmabuf_end_cpu_read,
    };
}
