#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]
// Without `std` the crate is `no_std`; its unit tests still link std (the harness, test-only
// helpers) while the code under test takes the `no_std` paths.
#![cfg_attr(not(any(feature = "std", test)), no_std)]

extern crate alloc;

pub mod buffer;
pub mod controls;
#[cfg(feature = "daedalus")]
pub mod daedalus;
pub mod format;
pub mod math;
pub mod metrics;
pub mod queue;
pub mod requirements;
pub mod simd;
pub mod sync;
pub mod transform;

pub mod prelude {
    pub use crate::{
        buffer::{
            BackendFrameMeta, CaptureInstant, ClockConversion, ClockSource, CpuAccess,
            FrameAllocation, FrameLatency, FrameLeaseDescriptor, FrameMeta, FrameMutability,
            FramePlaneDescriptor, FramePlaneShape, FrameResidency, FrameTiming,
            FrameValidationError, LibcameraFrameMeta, NativeFrameMeta, Plane, PlaneLayout,
            PlaneMut, ResidencyTransition, ResidencyTransitionReason, TimestampClock, UvcFrameMeta,
            V4l2FrameMeta, VisibleRow, VisibleRowMut, VisibleRows, VisibleRowsMut,
            plane_layout_from_dims, plane_layout_with_stride,
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

    pub use crate::{
        buffer::{
            BufferLease, BufferPool, BufferPoolMetrics, BufferPoolStats, CompanionKind,
            ExternalBacking, FrameLease, MemoryRegion, RegionHooks, box_downscale_luma,
            box_downscale_luma_in,
        },
        metrics::Metrics,
        transform::{
            FrameTransform, Rotation90, TransformError, TransformPoolConfig,
            TransformResidencyCapabilities, configure_transform_pool,
            packed_transform_residency_capabilities, transform_packed_frame, transform_pool_config,
            transform_pool_stats,
        },
    };

    pub use crate::queue::{
        BoundedRx, BoundedTx, DEFAULT_QUEUE_CAPACITY, QueueOverflow, QueueStats, RecvOutcome,
        RecvWaitOutcome, SendOutcome, SendWaitOutcome, bounded, bounded_with, default_bounded,
        newest,
    };

    #[cfg(all(feature = "std", unix))]
    pub use crate::buffer::{FrameBackingExport, FrameExportError, FrameFdPlane};

    #[cfg(all(feature = "std", target_os = "linux"))]
    pub use crate::buffer::{
        SharedBufferLease, SharedBufferPool, SharedBufferPoolStats, dmabuf_begin_cpu_read,
        dmabuf_begin_cpu_write, dmabuf_end_cpu_read, dmabuf_end_cpu_write,
    };
}
