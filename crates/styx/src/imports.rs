//! Task-focused import surfaces for callers that do not want the full facade prelude.

/// Minimal `FrameLease` import surface for core-only consumers.
pub mod framelease {
    pub use styx_core::prelude::{
        BackendFrameMeta, BufferLease, BufferPool, ExternalBacking, FourCc, FrameAllocation,
        FrameLatency, FrameLease, FrameLeaseDescriptor, FrameMeta, FramePlaneDescriptor,
        FrameResidency, FrameTiming, FrameValidationError, LibcameraFrameMeta, MediaFormat,
        PlaneLayout, Resolution, TimestampClock, V4l2FrameMeta, VisibleRow, VisibleRows,
    };

    #[cfg(unix)]
    pub use styx_core::prelude::{FrameBackingExport, FrameExportError, FrameFdPlane};

    #[cfg(target_os = "linux")]
    pub use styx_core::prelude::{SharedBufferLease, SharedBufferPool};
}

/// Capture request, backend, format, and frame receive APIs.
#[cfg(feature = "facade")]
pub mod capture {
    #[cfg(feature = "netcam")]
    pub use crate::capture_api::make_netcam_device;
    pub use crate::capture_api::{
        BackendConfig, CameraFormat, CameraIntervalPreference, CameraRequest, CameraStartPolicy,
        CaptureConfig, CaptureError, CaptureFrameIter, CaptureHandle, CaptureRequest,
        CaptureSource, CaptureStartPolicy, CaptureTunables, FileBackendConfig,
        LibcameraBufferMemory, LibcameraConfig, NetcamConfig, NetcamTunables, ReconnectPolicy,
        SelectedCamera, StyxConfig, TdnOutputMode, TransformConfig, V4l2Config,
        VirtualCaptureConfig, VirtualSourceConfig, make_virtual_device, make_virtual_rgb_device,
        open_best_camera, open_virtual_rgb, start_capture,
    };
    pub use crate::{BackendHandle, BackendKind, ProbedBackend, ProbedDevice};
    pub use styx_capture::prelude::{
        CaptureDescriptor, CaptureSource as CaptureSourceTrait, Mode, ModeId,
    };
    pub use styx_core::prelude::{
        ColorSpace, ControlId, ControlValue, FourCc, FrameLease, Interval, MediaFormat,
        RecvOutcome, RecvWaitOutcome, Resolution,
    };
}

/// Pipeline builder/runtime APIs.
#[cfg(feature = "facade")]
pub mod pipeline {
    pub use crate::memory::RuntimeMemoryReport;
    pub use crate::metrics::{HealthReport, PipelineMemoryStats, PipelineMetrics};
    pub use crate::session::{
        MediaPipeline, MediaPipelineBuilder, MediaPipelineFrameIter, PipelineExecutionMode,
    };
    pub use styx_core::prelude::{FrameLease, FrameTransform, RecvOutcome, RecvWaitOutcome};
}

/// Runtime codec selection and codec trait APIs.
#[cfg(feature = "facade")]
pub mod codec {
    pub use crate::runtime_codec::{
        CodecLatency, CodecOutputFormat, CodecSelector, CodecSelectorParseError, EncoderFamilySpec,
        FrameDecodePlan, FrameDecodePlanExt, RuntimeCodecCapability, RuntimeCodecInventory,
        codec_output_format_for_codec_selector, codec_output_format_for_encoder_selector,
        decode_to_rg24_for_format, default_decoder_codec_selector_for_capture_format,
        default_decoder_ids_by_capture_format, default_decoder_selector_for_capture_format,
        default_decoder_selectors_by_capture_format, default_stream_codec_selector,
        default_stream_encoder_selector, encoder_family_for_codec_selector,
        encoder_family_for_descriptor, encoder_family_for_selector,
        output_format_for_codec_selector, output_format_for_encoder_selector,
        runtime_codec_inventory, runtime_codec_inventory_with_config, shared_rg24_decode_bytes,
    };
    #[cfg(feature = "codec-jpeg-decoder")]
    pub use styx_codec::prelude::MjpegDecoder;
    pub use styx_codec::prelude::{
        Codec, CodecDescriptor, CodecError, CodecImplementationId, CodecKind, CodecPolicy,
        CodecPolicyBuilder, CodecRegistry, CodecRegistryConfig, CodecRegistryHandle,
        CodecResidencyCapabilities, CodecStats, RegistryError,
    };
    #[cfg(feature = "codec-ffmpeg")]
    pub use styx_codec::prelude::{
        FfmpegEncoderOptions, FfmpegH264Decoder, FfmpegH264Encoder, FfmpegH265Decoder,
        FfmpegH265Encoder, FfmpegMjpegDecoder, FfmpegMjpegEncoder,
    };
    pub use styx_core::prelude::{FourCc, FrameLease, MediaFormat, Resolution};
}

/// Graph pipeline APIs.
#[cfg(all(feature = "facade", feature = "daedalus-plugin"))]
pub mod graph {
    pub use crate::graph::{
        GraphPolicy, SinkNodeConfig, SinkPolicy, StyxCaptureSourceOptions, StyxCodecNodeDescriptor,
        StyxCodecNodeOptions, StyxControlEvent, StyxControlResult, StyxMediaPlugin,
        StyxSinkDescriptor, StyxSourceDescriptor, StyxSourceKind, bounded_blocking,
        bounded_drop_oldest, latest_only, register_camera_sources_all,
        register_camera_sources_limit, register_camera_sources_with_policy,
        register_capture_request_source_with_policy, register_capture_source_node,
        register_capture_source_node_with_options, register_control_types,
        register_frame_sink_node, register_framelease_type, register_network_stream_sink_node,
    };
}

/// Service event and lifecycle APIs.
#[cfg(feature = "facade")]
pub mod service {
    pub use crate::service::{
        PipelineWorkerEvent, PipelineWorkerStopReason, RecordingLifecycleEvent, ServiceEventCursor,
        ServiceEventPoll, SharedStyxServiceRuntime, SinkKind, SinkLifecycleEvent,
        StyxServiceConfig, StyxServiceEvent, StyxServiceRuntime, TimestampedServiceEvent,
    };
}

/// Device watch APIs.
#[cfg(feature = "facade")]
pub mod watch {
    #[cfg(all(feature = "hotplug", feature = "libcamera"))]
    pub use crate::watch::LibcameraHotplugWatcher;
    #[cfg(all(feature = "hotplug", target_os = "linux"))]
    pub use crate::watch::LinuxVideoFsWatcher;
    pub use crate::watch::{
        ChangedDevice, CompositeWatcher, DeviceWatchEvent, DeviceWatcher, InventoryDiff,
        InventoryEvent, InventoryEventCursor, InventoryEventPoll, InventoryEventRetentionStats,
        InventoryEventSubscription, WatchRefreshReport, WatchRuntime, WatchRuntimeConfig,
    };
}

/// Lossless stream recording and replay.
#[cfg(feature = "facade")]
pub mod replay {
    #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
    pub use crate::replay::StreamRecorder;
    pub use crate::replay::{
        RecordingFrames, RecordingHeader, ReplayError, ReplayPacing, ReplaySourceConfig,
        StreamFormat, open_recording, read_header,
    };
}

/// Recording APIs.
#[cfg(all(feature = "facade", feature = "hooks"))]
pub mod recording {
    pub use crate::recording::{
        FrameRecorder, RecordingError, RecordingFormat, RecordingFrameIndexEntry, RecordingOptions,
        RecordingSessionMetadata,
    };
}
