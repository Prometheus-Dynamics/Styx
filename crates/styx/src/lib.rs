#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]

#[cfg(any(feature = "v4l2", feature = "libcamera"))]
use std::collections::HashSet;
#[cfg(feature = "v4l2")]
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(any(feature = "file-backend", feature = "simulation-bevy"))]
use std::path::PathBuf;

pub use styx_capture as capture;
pub use styx_codec as codec;
pub use styx_core as core;
#[cfg(feature = "libcamera")]
pub use styx_libcamera as libcamera;
#[cfg(feature = "v4l2")]
pub use styx_v4l2 as v4l2;
#[cfg(feature = "preview-window")]
pub mod preview;

pub use thiserror;

pub mod capabilities;
pub mod capture_api;
mod device_identity;
mod frame_sizing;
#[cfg(feature = "daedalus-plugin")]
pub mod graph;
pub mod memory;
mod metrics;
#[cfg(feature = "hooks")]
pub mod recording;
pub mod runtime_codec;
#[cfg(feature = "serde")]
mod serde_impls;
pub mod service;
pub mod session;
#[cfg(feature = "simulation-bevy")]
pub mod simulation;
pub mod watch;

#[cfg(feature = "preview-window")]
pub mod extras {
    pub mod preview_window {
        pub use crate::preview::PreviewWindow;
    }
}

/// Task-focused import surfaces for callers that do not want the full facade prelude.
pub mod imports {
    /// Capture request, backend, format, and frame receive APIs.
    pub mod capture {
        #[cfg(feature = "netcam")]
        pub use crate::capture_api::make_netcam_device;
        pub use crate::capture_api::{
            BackendConfig, CameraFormat, CameraIntervalPreference, CameraRequest,
            CameraStartPolicy, CaptureConfig, CaptureError, CaptureFrameIter, CaptureHandle,
            CaptureRequest, CaptureSource, CaptureStartPolicy, CaptureTunables, FileBackendConfig,
            LibcameraConfig, NetcamConfig, NetcamTunables, SelectedCamera, StyxConfig,
            TdnOutputMode, TransformConfig, V4l2Config, VirtualCaptureConfig, VirtualSourceConfig,
            make_virtual_device, make_virtual_rgb_device, open_best_camera, open_virtual_rgb,
            start_capture,
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
    pub mod pipeline {
        pub use crate::memory::RuntimeMemoryReport;
        pub use crate::metrics::{HealthReport, PipelineMemoryStats, PipelineMetrics};
        pub use crate::session::{
            MediaPipeline, MediaPipelineBuilder, MediaPipelineFrameIter, PipelineExecutionMode,
        };
        pub use styx_core::prelude::{FrameLease, FrameTransform, RecvOutcome, RecvWaitOutcome};
    }

    /// Runtime codec selection and codec trait APIs.
    pub mod codec {
        pub use crate::runtime_codec::{
            CodecLatency, CodecOutputFormat, CodecSelector, CodecSelectorParseError,
            EncoderFamilySpec, FrameDecodePlan, FrameDecodePlanExt, RuntimeCodecCapability,
            RuntimeCodecInventory, codec_output_format_for_codec_selector,
            codec_output_format_for_encoder_selector, decode_to_rg24_for_format,
            default_decoder_codec_selector_for_capture_format,
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
        pub use styx_core::prelude::{FourCc, FrameLease, MediaFormat, Resolution};
    }

    /// Graph pipeline APIs.
    #[cfg(feature = "daedalus-plugin")]
    pub mod graph {
        pub use crate::graph::{
            GraphPolicy, SinkNodeConfig, SinkPolicy, StyxCaptureSourceOptions,
            StyxCodecNodeDescriptor, StyxCodecNodeOptions, StyxControlEvent, StyxControlResult,
            StyxMediaPlugin, StyxSinkDescriptor, StyxSourceDescriptor, StyxSourceKind,
            bounded_blocking, bounded_drop_oldest, latest_only, register_camera_sources_all,
            register_camera_sources_limit, register_camera_sources_with_policy,
            register_capture_request_source_with_policy, register_capture_source_node,
            register_capture_source_node_with_options, register_control_types,
            register_frame_sink_node, register_framelease_type, register_network_stream_sink_node,
        };
    }

    /// Service event and lifecycle APIs.
    pub mod service {
        pub use crate::service::{
            PipelineWorkerEvent, PipelineWorkerStopReason, RecordingLifecycleEvent,
            ServiceEventCursor, ServiceEventPoll, SharedStyxServiceRuntime, SinkKind,
            SinkLifecycleEvent, StyxServiceConfig, StyxServiceEvent, StyxServiceRuntime,
            TimestampedServiceEvent,
        };
    }

    /// Device watch APIs.
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

    /// Recording APIs.
    #[cfg(feature = "hooks")]
    pub mod recording {
        pub use crate::recording::{
            FrameRecorder, RecordingError, RecordingFormat, RecordingFrameIndexEntry,
            RecordingOptions, RecordingSessionMetadata,
        };
    }
}

/// Unified device descriptor for probed backends.
///
/// # Example
/// ```rust
/// use styx::prelude::*;
///
/// let dev = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
/// assert_eq!(dev.identity.display, "virtual");
/// assert_eq!(dev.backends.len(), 1);
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ProbedDevice {
    pub identity: DeviceIdentity,
    pub backends: Vec<ProbedBackend>,
}

/// Backend-specific entry for a probed device.
///
/// # Example
/// ```rust
/// use styx::prelude::*;
///
/// let dev = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
/// let backend = &dev.backends[0];
/// assert_eq!(backend.kind, BackendKind::Virtual);
/// assert_eq!(backend.descriptor.modes.len(), 1);
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ProbedBackend {
    pub kind: BackendKind,
    pub handle: BackendHandle,
    pub descriptor: styx_capture::CaptureDescriptor,
    pub properties: Vec<(String, String)>,
}

/// Known backend kinds.
///
/// The `Virtual`/`Netcam`/`File` kinds map to synthetic backends created via
/// helpers in `styx::capture_api`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum BackendKind {
    V4l2,
    Libcamera,
    Virtual,
    Netcam,
    File,
    Simulation,
}

/// Backend-specific handle used for configuration/streaming.
///
/// # Example
/// ```rust
/// use styx::prelude::*;
///
/// let dev = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
/// let handle = &dev.backends[0].handle;
/// assert_eq!(handle.kind(), BackendKind::Virtual);
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum BackendHandle {
    #[cfg(feature = "v4l2")]
    V4l2 {
        path: String,
    },
    #[cfg(feature = "libcamera")]
    Libcamera {
        id: String,
    },
    Virtual,
    #[cfg(feature = "netcam")]
    Netcam {
        url: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    #[cfg(feature = "file-backend")]
    File {
        #[cfg_attr(feature = "schema", schema(value_type = Vec<String>))]
        paths: Vec<PathBuf>,
        fps: u32,
        loop_forever: bool,
    },
    #[cfg(feature = "simulation-bevy")]
    Simulation {
        #[cfg_attr(feature = "schema", schema(value_type = String))]
        scene_path: PathBuf,
        config: crate::simulation::SimulationDeviceConfig,
    },
}

impl BackendHandle {
    /// Return the backend kind for this handle.
    pub fn kind(&self) -> BackendKind {
        match self {
            #[cfg(feature = "v4l2")]
            BackendHandle::V4l2 { .. } => BackendKind::V4l2,
            #[cfg(feature = "libcamera")]
            BackendHandle::Libcamera { .. } => BackendKind::Libcamera,
            BackendHandle::Virtual => BackendKind::Virtual,
            #[cfg(feature = "netcam")]
            BackendHandle::Netcam { .. } => BackendKind::Netcam,
            #[cfg(feature = "file-backend")]
            BackendHandle::File { .. } => BackendKind::File,
            #[cfg(feature = "simulation-bevy")]
            BackendHandle::Simulation { .. } => BackendKind::Simulation,
        }
    }
}

impl ProbedDevice {
    /// Return the first advertised backend for this device.
    pub fn default_backend(&self) -> Option<&ProbedBackend> {
        self.backends.first()
    }

    /// Return a backend by kind.
    pub fn backend(&self, kind: BackendKind) -> Option<&ProbedBackend> {
        self.backends.iter().find(|backend| backend.kind == kind)
    }

    /// Return the descriptor for the first advertised backend.
    pub fn default_descriptor(&self) -> Option<&styx_capture::CaptureDescriptor> {
        self.default_backend().map(|backend| &backend.descriptor)
    }

    /// Return the first advertised mode from the first advertised backend.
    pub fn default_mode(&self) -> Option<&styx_capture::Mode> {
        self.default_descriptor()
            .and_then(|descriptor| descriptor.modes.first())
    }

    /// Return the first advertised mode for a backend kind.
    pub fn mode_for_backend(&self, kind: BackendKind) -> Option<&styx_capture::Mode> {
        self.backend(kind)
            .and_then(|backend| backend.descriptor.modes.first())
    }

    /// Build a capture request pinned to the default mode when one is advertised.
    pub fn capture_request(&self) -> crate::capture_api::CaptureRequest<'_> {
        let request = crate::capture_api::CaptureRequest::new(self);
        if let Some(mode) = self.default_mode() {
            request.mode(mode.id.clone())
        } else {
            request
        }
    }

    /// Open capture using the default backend/mode selection.
    pub fn open(
        &self,
    ) -> Result<crate::capture_api::CaptureHandle, crate::capture_api::CaptureError> {
        self.capture_request().start()
    }

    /// Open capture with request-local runtime configuration.
    pub fn open_with_config(
        &self,
        config: crate::capture_api::StyxConfig,
    ) -> Result<crate::capture_api::CaptureHandle, crate::capture_api::CaptureError> {
        self.capture_request().config(config).start()
    }

    /// Open capture using a resilient or caller-supplied start policy.
    pub fn open_with_policy(
        &self,
        policy: crate::capture_api::CaptureStartPolicy,
    ) -> Result<crate::capture_api::CaptureHandle, crate::capture_api::CaptureError> {
        self.capture_request().start_with_policy(policy)
    }

    /// Build a media pipeline from this device without manually constructing a request.
    pub fn pipeline(&self) -> crate::session::MediaPipelineBuilder<'_> {
        crate::session::MediaPipelineBuilder::new(self.capture_request())
    }
}

/// Physical device identity derived from fingerprints/props.
///
/// `display` is a human-friendly string, while `keys` contains fingerprints
/// that help merge identical devices across backends.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct DeviceIdentity {
    /// Display-friendly identifier.
    pub display: String,
    /// Fingerprint keys used for matching.
    pub keys: Vec<String>,
}

/// Backend-specific probe failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BackendProbeError {
    pub backend: BackendKind,
    pub message: String,
}

impl BackendProbeError {
    pub fn new(backend: BackendKind, message: impl Into<String>) -> Self {
        Self {
            backend,
            message: message.into(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.message.is_empty()
    }
}

impl std::fmt::Display for BackendProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.backend, self.message)
    }
}

impl std::error::Error for BackendProbeError {}

impl From<(BackendKind, String)> for BackendProbeError {
    fn from((backend, message): (BackendKind, String)) -> Self {
        Self::new(backend, message)
    }
}

impl From<(BackendKind, &str)> for BackendProbeError {
    fn from((backend, message): (BackendKind, &str)) -> Self {
        Self::new(backend, message)
    }
}

impl From<&str> for BackendProbeError {
    fn from(value: &str) -> Self {
        parse_backend_probe_error(value)
            .unwrap_or_else(|| Self::new(BackendKind::Virtual, value.to_string()))
    }
}

impl From<String> for BackendProbeError {
    fn from(value: String) -> Self {
        parse_backend_probe_error(&value).unwrap_or_else(|| Self::new(BackendKind::Virtual, value))
    }
}

impl PartialEq<str> for BackendProbeError {
    fn eq(&self, other: &str) -> bool {
        other.strip_prefix(backend_error_prefix(self.backend)) == Some(self.message.as_str())
    }
}

impl PartialEq<&str> for BackendProbeError {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BackendKind::V4l2 => "v4l2",
            BackendKind::Libcamera => "libcamera",
            BackendKind::Virtual => "virtual",
            BackendKind::Netcam => "netcam",
            BackendKind::File => "file",
            BackendKind::Simulation => "simulation",
        })
    }
}

impl std::str::FromStr for BackendKind {
    type Err = BackendKindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "v4l2" | "video4linux" | "video4linux2" => Ok(BackendKind::V4l2),
            "libcamera" => Ok(BackendKind::Libcamera),
            "virtual" => Ok(BackendKind::Virtual),
            "netcam" | "network" | "network-camera" => Ok(BackendKind::Netcam),
            "file" | "file-backend" => Ok(BackendKind::File),
            "simulation" | "simulation-bevy" => Ok(BackendKind::Simulation),
            _ => Err(BackendKindParseError {
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendKindParseError {
    value: String,
}

impl std::fmt::Display for BackendKindParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown backend kind: {}", self.value)
    }
}

impl std::error::Error for BackendKindParseError {}

/// Probe result that includes any backend errors encountered.
///
/// # Example
/// ```rust
/// use styx::probe_all_with_errors;
///
/// let res = probe_all_with_errors();
/// assert!(res.errors.iter().all(|err| !err.is_empty()));
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProbeResult {
    pub devices: Vec<ProbedDevice>,
    pub errors: Vec<BackendProbeError>,
}

/// Probe all enabled backends and return a merged list.
///
/// # Example
/// ```rust
/// use styx::prelude::*;
///
/// let devices = probe_all();
/// assert!(devices.iter().all(|device| !device.identity.display.is_empty()));
/// ```
pub fn probe_all() -> Vec<ProbedDevice> {
    probe_all_with_errors().devices
}

/// Probe all enabled backends using explicit runtime configuration.
pub fn probe_all_with_config(config: &capture_api::StyxConfig) -> Vec<ProbedDevice> {
    probe_all_with_errors_with_config(config).devices
}

/// Probe all enabled backends and include any probe errors.
///
/// Prefer this when you want observability into backend failures.
pub fn probe_all_with_errors() -> ProbeResult {
    probe_all_with_errors_with_options(false)
}

/// Probe all enabled backends using explicit runtime configuration and include any probe errors.
pub fn probe_all_with_errors_with_config(config: &capture_api::StyxConfig) -> ProbeResult {
    probe_backends_with_errors_with_options(false, None, Some(config))
}

pub(crate) fn probe_all_with_errors_with_options(_force_refresh: bool) -> ProbeResult {
    probe_backends_with_errors_with_options(
        _force_refresh,
        Some(&[
            #[cfg(feature = "v4l2")]
            BackendKind::V4l2,
            #[cfg(feature = "libcamera")]
            BackendKind::Libcamera,
        ]),
        None,
    )
}

pub(crate) fn probe_backends_with_errors_with_options(
    _force_refresh: bool,
    _backends: Option<&[BackendKind]>,
    _config: Option<&capture_api::StyxConfig>,
) -> ProbeResult {
    // Backend feature gates may leave the probe accumulators untouched in minimal builds.
    #[allow(unused_mut)]
    let mut devices: Vec<ProbedDevice> = Vec::new();
    // Backend feature gates may leave the probe accumulators untouched in minimal builds.
    #[allow(unused_mut)]
    let mut errors: Vec<BackendProbeError> = Vec::new();

    #[cfg(feature = "v4l2")]
    if _backends.is_none_or(|backends| backends.contains(&BackendKind::V4l2)) {
        let (v4l2_devices, v4l2_errors) =
            match catch_unwind(AssertUnwindSafe(styx_v4l2::probe_devices)) {
                Ok(res) => res,
                Err(_) => (Vec::new(), vec!["probe panicked".to_string()]),
            };
        errors.extend(
            v4l2_errors
                .into_iter()
                .map(|error| BackendProbeError::new(BackendKind::V4l2, error)),
        );
        for dev in v4l2_devices {
            let backend = ProbedBackend {
                kind: BackendKind::V4l2,
                handle: BackendHandle::V4l2 {
                    path: dev.path.clone(),
                },
                descriptor: dev.descriptor,
                properties: dev.properties,
            };
            merge_backend(&mut devices, dev.path.clone(), backend);
        }
    }
    #[cfg(feature = "libcamera")]
    if _backends.is_none_or(|backends| backends.contains(&BackendKind::Libcamera)) {
        if let Some(config) = _config {
            let tunables = config.libcamera_config();
            styx_libcamera::set_manager_config(styx_libcamera::LibcameraManagerConfig {
                probe_cache_ttl_ms: tunables.probe_cache_ttl_ms,
            });
        }
        let (libcamera_devices, libcamera_errors) = if _force_refresh {
            styx_libcamera::probe_devices_uncached_with_errors()
        } else {
            styx_libcamera::probe_devices_with_errors()
        };
        errors.extend(
            libcamera_errors
                .into_iter()
                .map(|error| BackendProbeError::new(BackendKind::Libcamera, error)),
        );
        for dev in libcamera_devices {
            let backend = ProbedBackend {
                kind: BackendKind::Libcamera,
                handle: BackendHandle::Libcamera { id: dev.id.clone() },
                descriptor: dev.descriptor,
                properties: dev.properties,
            };
            merge_backend(&mut devices, dev.id.clone(), backend);
        }
    }
    ProbeResult { devices, errors }
}

#[cfg(any(feature = "v4l2", feature = "libcamera"))]
fn merge_backend(devices: &mut Vec<ProbedDevice>, id: String, backend: ProbedBackend) {
    let new_keys: HashSet<String> = device_identity::derive_keys(&id, &backend.properties)
        .into_iter()
        .collect();
    let new_keys_vec: Vec<String> = new_keys.iter().cloned().collect();
    if let Some(existing) = devices
        .iter_mut()
        .find(|d| d.identity.keys.iter().any(|k| new_keys.contains(k)))
    {
        existing.backends.push(backend);
        for k in new_keys {
            if existing.identity.keys.iter().any(|ek| ek == &k) {
                continue;
            }
            existing.identity.keys.push(k);
        }
    } else {
        devices.push(ProbedDevice {
            identity: DeviceIdentity {
                display: device_identity::pick_display_id(&id, &backend.properties),
                keys: new_keys_vec,
            },
            backends: vec![backend],
        });
    }
}

fn backend_error_prefix(backend: BackendKind) -> &'static str {
    match backend {
        BackendKind::V4l2 => "v4l2: ",
        BackendKind::Libcamera => "libcamera: ",
        BackendKind::Virtual => "virtual: ",
        BackendKind::Netcam => "netcam: ",
        BackendKind::File => "file: ",
        BackendKind::Simulation => "simulation: ",
    }
}

fn parse_backend_probe_error(value: &str) -> Option<BackendProbeError> {
    [
        BackendKind::V4l2,
        BackendKind::Libcamera,
        BackendKind::Virtual,
        BackendKind::Netcam,
        BackendKind::File,
        BackendKind::Simulation,
    ]
    .into_iter()
    .find_map(|backend| {
        value
            .strip_prefix(backend_error_prefix(backend))
            .map(|message| BackendProbeError::new(backend, message))
    })
}

pub mod prelude {
    pub use crate::capabilities::{
        CaptureBackendCapability, CodecCapability, CrossProcessExportMode, FrameBackingCapability,
        StyxCapabilityInventory, StyxPathPlan, StyxPathRequest, TransformCapability,
        explain_styx_path, styx_capability_inventory,
    };
    #[cfg(feature = "file-backend")]
    pub use crate::capture_api::FileSourceConfig;
    #[cfg(feature = "netcam")]
    pub use crate::capture_api::NetcamSourceConfig;
    pub use crate::capture_api::{
        BackendConfig, CameraFormat, CameraIntervalPreference, CameraRequest, CameraStartPolicy,
        CaptureConfig, CaptureError, CaptureFrameIter, CaptureHandle, CaptureRequest,
        CaptureSource, CaptureStartPolicy, CaptureTunables, FileBackendConfig, LibcameraConfig,
        NetcamConfig, NetcamTunables, SelectedCamera, StyxConfig, TdnOutputMode, TransformConfig,
        V4l2Config, VirtualCaptureConfig, VirtualSourceConfig, open_best_camera, open_virtual_rgb,
        start_capture,
    };
    #[cfg(all(feature = "daedalus-plugin", feature = "hooks"))]
    pub use crate::graph::register_file_sequence_sink_node;
    #[cfg(feature = "daedalus-plugin")]
    pub use crate::graph::{
        CONTROL_EVENT_TYPE_KEY, CONTROL_RESULT_TYPE_KEY, FRAMELEASE_TYPE_KEY, GraphPolicy,
        SinkNodeConfig, SinkPolicy, StyxCaptureSourceOptions, StyxCodecNodeDescriptor,
        StyxCodecNodeOptions, StyxControlEvent, StyxControlResult, StyxMediaPlugin,
        StyxSinkDescriptor, StyxSourceDescriptor, StyxSourceKind, bounded_blocking,
        bounded_drop_oldest, concrete_codec_node_id, control_event_payload, control_event_type_key,
        control_result_type_key, framelease_daedalus_residency, framelease_payload,
        framelease_type_key, latest_only, register_camera_sources_all,
        register_camera_sources_limit, register_camera_sources_with_policy,
        register_capture_request_source_with_policy, register_capture_source_node,
        register_capture_source_node_with_options, register_control_types,
        register_frame_sink_node, register_framelease_type, register_network_stream_sink_node,
    };
    pub use crate::memory::{
        FdClass, FdClassStats, FdInventoryStats, FdTargetStats, KernelDmabufStats, MappingCategory,
        MappingCategoryStats, MappingNameStats, ProcessMemoryStats, RuntimeMemoryReport,
        runtime_memory_report, runtime_memory_report_with_styx,
    };
    pub use crate::metrics::{
        CopyMetrics, CopyStats, FrameDropReason, FrameDropStats, GraphTelemetryStats, HealthReport,
        PipelineMemoryStats, PipelineMetrics, PipelineStage, PipelineStageError,
        QueueTelemetryStats, ResidencyMetrics, ResidencySnapshot, StageErrorMetrics, StageMetrics,
        StageSnapshot,
    };
    #[cfg(feature = "hooks")]
    pub use crate::recording::{
        FrameRecorder, RecordingError, RecordingFormat, RecordingFrameIndexEntry, RecordingOptions,
        RecordingSessionMetadata,
    };
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
        runtime_codec_inventory, shared_rg24_decode_bytes,
    };
    pub use crate::service::{
        PipelineWorkerEvent, PipelineWorkerStopReason, RecordingLifecycleEvent, ServiceEventCursor,
        ServiceEventPoll, SharedStyxServiceRuntime, SinkKind, SinkLifecycleEvent,
        StyxServiceConfig, StyxServiceEvent, StyxServiceRuntime, TimestampedServiceEvent,
    };
    pub use crate::session::{
        MediaPipeline, MediaPipelineBuilder, MediaPipelineFrameIter, PipelineExecutionMode,
    };
    #[cfg(all(feature = "hotplug", feature = "libcamera"))]
    pub use crate::watch::LibcameraHotplugWatcher;
    #[cfg(all(feature = "hotplug", target_os = "linux"))]
    pub use crate::watch::LinuxVideoFsWatcher;
    pub use crate::watch::{
        ChangedDevice, CompositeWatcher, DeviceWatchEvent, DeviceWatcher, InventoryDiff,
        InventoryEvent, InventoryEventCursor, InventoryEventPoll, InventoryEventRetentionStats,
        InventoryEventSubscription, WatchRefreshReport, WatchRuntime, WatchRuntimeConfig,
    };
    pub use crate::{BackendHandle, BackendKind, ProbedBackend, ProbedDevice};
    pub use crate::{BackendKindParseError, BackendProbeError};
    pub use crate::{probe_all, probe_all_with_config, probe_all_with_errors_with_config};
    #[cfg(feature = "daedalus-plugin")]
    pub use daedalus::engine::MetricsLevel as GraphMetricsLevel;
    pub use styx_capture::prelude::*;
    pub use styx_codec::prelude::*;
    // Release policy: keep the facade prelude stable even when downstream crates use only a
    // subset of the re-exported core primitives in a given feature combination.
    #[allow(unused_imports)]
    pub use styx_core::prelude::*;
    pub use styx_core::prelude::{FrameTransform, Rotation90};
    #[cfg(feature = "libcamera")]
    pub use styx_libcamera::prelude::{
        LibcameraCapture, LibcameraDeviceInfo, probe_devices as probe_libcamera,
    };
    #[cfg(feature = "v4l2")]
    pub use styx_v4l2::prelude::{V4l2DeviceInfo, probe_devices as probe_v4l2};
}

#[cfg(test)]
mod tests {
    use super::{BackendKind, BackendProbeError};

    #[test]
    fn backend_kind_display_and_parse_round_trip() {
        for backend in [
            BackendKind::V4l2,
            BackendKind::Libcamera,
            BackendKind::Virtual,
            BackendKind::Netcam,
            BackendKind::File,
            BackendKind::Simulation,
        ] {
            assert_eq!(backend.to_string().parse::<BackendKind>(), Ok(backend));
        }
    }

    #[test]
    fn backend_probe_error_parses_legacy_prefixed_message() {
        let error = BackendProbeError::from("libcamera: camera manager failed");

        assert_eq!(error.backend, BackendKind::Libcamera);
        assert_eq!(error.message, "camera manager failed");
        assert_eq!(error.to_string(), "libcamera: camera manager failed");
    }
}
