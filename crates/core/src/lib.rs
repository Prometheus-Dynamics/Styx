#![doc = include_str!("../README.md")]

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
            ExternalBacking, FrameLease, FrameMeta, FrameMutability, FrameResidency, Plane,
            PlaneLayout, PlaneMut, ResidencyTransition, ResidencyTransitionReason, V4l2FrameMeta,
            plane_layout_from_dims, plane_layout_with_stride,
        },
        controls::{Access, ControlId, ControlKind, ControlMeta, ControlMetadata, ControlValue},
        format::{ColorSpace, FourCc, Interval, IntervalStepwise, MediaFormat, Resolution},
        metrics::Metrics,
        queue::{
            BoundedRx, BoundedTx, QueueStats, RecvOutcome, RecvWaitOutcome, SendOutcome,
            SendWaitOutcome, bounded, newest, unbounded,
        },
        transform::{
            FrameTransform, Rotation90, TransformError, TransformResidencyCapabilities,
            packed_transform_residency_capabilities, transform_packed_frame,
        },
    };
}
