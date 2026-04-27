use crate::{BackendKind, ProbedBackend, ProbedDevice};
use std::time::Duration;
use styx_capture::prelude::*;

use super::handle::start_backend;

/// Errors starting a capture session.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let device = probe_all().into_iter().next().expect("device");
/// let err = CaptureRequest::new(&device)
///     .backend(BackendKind::Virtual)
///     .start()
///     .err()
///     .expect("error");
/// eprintln!("capture failed: {} ({})", err, err.code());
/// ```
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("device has no backends")]
    NoBackend,
    #[error("backend {0:?} not available on this device")]
    BackendUnavailable(BackendKind),
    #[error("backend {0:?} not implemented in this build")]
    BackendMissing(BackendKind),
    #[error("no modes advertised by backend")]
    NoModes,
    #[error("no camera matched request")]
    NoCameraMatchingRequest,
    #[error("mode {0:?} not advertised by backend")]
    InvalidMode(ModeId),
    #[error("capture config rejected: {0}")]
    InvalidConfig(String),
    #[error("backend {0:?} capture not implemented yet")]
    NotImplemented(BackendKind),
    #[error("control plane not available for backend")]
    ControlUnsupported,
    #[error("control apply failed: {message}")]
    ControlApply {
        kind: ControlApplyKind,
        message: String,
    },
    #[error("libcamera camera not found: requested={requested}, seen={seen:?}")]
    LibcameraCameraNotFound {
        requested: String,
        seen: Vec<String>,
    },
    #[error("libcamera backend busy: {0}")]
    LibcameraBusy(String),
    #[error("libcamera generate_configuration failed")]
    LibcameraGenerateConfigurationFailed,
    #[error("libcamera TDN output stream unavailable")]
    LibcameraTdnOutputUnavailable,
    #[error("libcamera TDN configuration mismatch: {0}")]
    LibcameraTdnConfigurationMismatch(String),
    #[error("backend error: {0}")]
    Backend(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlApplyKind {
    Other,
    SetControlsRejected,
    InvalidArgument,
    PermissionDenied,
}

/// TDN output stream selection policy (libcamera PiSP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum TdnOutputMode {
    Off,
    #[default]
    Auto,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureStartPolicy {
    pub max_attempts: usize,
    pub retry_backoff: Duration,
    pub retry_transient_errors: bool,
    pub retry_without_controls_on_control_errors: bool,
    pub retry_with_tdn_disabled: bool,
}

impl Default for CaptureStartPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            retry_backoff: Duration::ZERO,
            retry_transient_errors: false,
            retry_without_controls_on_control_errors: false,
            retry_with_tdn_disabled: false,
        }
    }
}

impl CaptureStartPolicy {
    pub fn resilient() -> Self {
        Self {
            max_attempts: 30,
            retry_backoff: Duration::from_millis(250),
            retry_transient_errors: true,
            retry_without_controls_on_control_errors: true,
            retry_with_tdn_disabled: true,
        }
    }
}

impl CaptureError {
    pub fn control_apply(message: impl Into<String>) -> Self {
        Self::ControlApply {
            kind: ControlApplyKind::Other,
            message: message.into(),
        }
    }

    pub fn classified_control_apply(kind: ControlApplyKind, message: impl Into<String>) -> Self {
        Self::ControlApply {
            kind,
            message: message.into(),
        }
    }

    /// Stable string code for error classification.
    pub fn code(&self) -> &'static str {
        match self {
            CaptureError::NoBackend => "no_backend",
            CaptureError::BackendUnavailable(_) => "backend_unavailable",
            CaptureError::BackendMissing(_) => "backend_missing",
            CaptureError::NoModes => "no_modes",
            CaptureError::NoCameraMatchingRequest => "no_camera_matching_request",
            CaptureError::InvalidMode(_) => "invalid_mode",
            CaptureError::InvalidConfig(_) => "invalid_config",
            CaptureError::NotImplemented(_) => "not_implemented",
            CaptureError::ControlUnsupported => "control_unsupported",
            CaptureError::ControlApply { .. } => "control_apply_failed",
            CaptureError::LibcameraCameraNotFound { .. } => "libcamera_camera_not_found",
            CaptureError::LibcameraBusy(_) => "libcamera_busy",
            CaptureError::LibcameraGenerateConfigurationFailed => {
                "libcamera_generate_configuration_failed"
            }
            CaptureError::LibcameraTdnOutputUnavailable => "libcamera_tdn_output_unavailable",
            CaptureError::LibcameraTdnConfigurationMismatch(_) => {
                "libcamera_tdn_configuration_mismatch"
            }
            CaptureError::Backend(_) => "backend_error",
        }
    }

    /// Whether the error may succeed when retried.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            CaptureError::BackendUnavailable(_)
                | CaptureError::LibcameraCameraNotFound { .. }
                | CaptureError::LibcameraBusy(_)
                | CaptureError::LibcameraGenerateConfigurationFailed
                | CaptureError::LibcameraTdnOutputUnavailable
                | CaptureError::Backend(_)
        )
    }

    /// Whether a start/reconfigure failure is worth retrying after a short backoff.
    pub fn is_transient_start(&self) -> bool {
        matches!(
            self,
            CaptureError::LibcameraCameraNotFound { .. }
                | CaptureError::LibcameraBusy(_)
                | CaptureError::LibcameraGenerateConfigurationFailed
                | CaptureError::LibcameraTdnOutputUnavailable
        )
    }

    /// Whether the caller should retry with libcamera TDN disabled.
    pub fn requires_disabling_tdn(&self) -> bool {
        matches!(
            self,
            CaptureError::LibcameraTdnOutputUnavailable
                | CaptureError::LibcameraTdnConfigurationMismatch(_)
        )
    }

    /// Whether the caller should retry without the requested controls.
    pub fn requires_dropping_controls(&self) -> bool {
        matches!(
            self,
            CaptureError::ControlApply {
                kind: ControlApplyKind::SetControlsRejected
                    | ControlApplyKind::InvalidArgument
                    | ControlApplyKind::PermissionDenied,
                ..
            }
        )
    }
}

const DEFAULT_CAMERA_FORMATS: &[FourCc] = &[
    FourCc::new(*b"RG24"),
    FourCc::new(*b"RGB3"),
    FourCc::new(*b"BGR3"),
    FourCc::new(*b"BG24"),
    FourCc::new(*b"RGBA"),
    FourCc::new(*b"BGRA"),
    FourCc::new(*b"NV12"),
    FourCc::new(*b"YUYV"),
    FourCc::new(*b"I420"),
    FourCc::new(*b"MJPG"),
    FourCc::new(*b"JPEG"),
    FourCc::new(*b"R8  "),
    FourCc::new(*b"GREY"),
];

const DEFAULT_CAMERA_BACKEND_PRIORITY: &[BackendKind] = &[
    BackendKind::V4l2,
    BackendKind::Libcamera,
    BackendKind::Virtual,
    BackendKind::Netcam,
    BackendKind::File,
    BackendKind::Simulation,
];

pub trait CameraFormat {
    fn into_fourcc(self) -> FourCc;
}

impl CameraFormat for FourCc {
    fn into_fourcc(self) -> FourCc {
        self
    }
}

impl CameraFormat for [u8; 4] {
    fn into_fourcc(self) -> FourCc {
        FourCc::new(self)
    }
}

impl CameraFormat for &[u8; 4] {
    fn into_fourcc(self) -> FourCc {
        FourCc::new(*self)
    }
}

impl CameraFormat for &str {
    fn into_fourcc(self) -> FourCc {
        self.parse()
            .expect("camera format strings must be exactly four bytes")
    }
}

impl CameraFormat for String {
    fn into_fourcc(self) -> FourCc {
        self.as_str().into_fourcc()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraIntervalPreference {
    Default,
    Fastest,
    Slowest,
    Exact(Interval),
    None,
}

pub type CameraStartPolicy = CaptureStartPolicy;

/// High-level camera selector for the common "open the best usable camera" path.
///
/// `CameraRequest` keeps the same control as `CaptureRequest`, but moves probing,
/// backend choice, mode choice, and interval choice into one reusable policy.
#[derive(Debug, Clone)]
pub struct CameraRequest {
    devices: Vec<ProbedDevice>,
    format_priority: Vec<FourCc>,
    backend_priority: Vec<BackendKind>,
    resolution_priority: Vec<(u32, u32)>,
    min_width: Option<u32>,
    min_height: Option<u32>,
    max_width: Option<u32>,
    max_height: Option<u32>,
    interval_preference: CameraIntervalPreference,
    controls: Vec<(ControlId, ControlValue)>,
    tdn_output_mode: TdnOutputMode,
}

impl CameraRequest {
    pub fn new() -> Self {
        Self {
            devices: crate::probe_all(),
            format_priority: DEFAULT_CAMERA_FORMATS.to_vec(),
            backend_priority: DEFAULT_CAMERA_BACKEND_PRIORITY.to_vec(),
            resolution_priority: Vec::new(),
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            interval_preference: CameraIntervalPreference::Fastest,
            controls: Vec::new(),
            tdn_output_mode: TdnOutputMode::default(),
        }
    }

    pub fn from_devices(devices: Vec<ProbedDevice>) -> Self {
        Self {
            devices,
            ..Self::empty()
        }
    }

    fn empty() -> Self {
        Self {
            devices: Vec::new(),
            format_priority: DEFAULT_CAMERA_FORMATS.to_vec(),
            backend_priority: DEFAULT_CAMERA_BACKEND_PRIORITY.to_vec(),
            resolution_priority: Vec::new(),
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            interval_preference: CameraIntervalPreference::Fastest,
            controls: Vec::new(),
            tdn_output_mode: TdnOutputMode::default(),
        }
    }

    pub fn format_priority<T: CameraFormat>(
        mut self,
        formats: impl IntoIterator<Item = T>,
    ) -> Self {
        self.format_priority = formats.into_iter().map(CameraFormat::into_fourcc).collect();
        self
    }

    pub fn backend_priority(mut self, priority: impl IntoIterator<Item = BackendKind>) -> Self {
        self.backend_priority = priority.into_iter().collect();
        self
    }

    pub fn resolution_priority(mut self, priority: impl IntoIterator<Item = (u32, u32)>) -> Self {
        self.resolution_priority = priority.into_iter().collect();
        self
    }

    pub fn min_resolution(mut self, width: u32, height: u32) -> Self {
        self.min_width = Some(width);
        self.min_height = Some(height);
        self
    }

    pub fn max_resolution(mut self, width: u32, height: u32) -> Self {
        self.max_width = Some(width);
        self.max_height = Some(height);
        self
    }

    pub fn interval_preference(mut self, preference: CameraIntervalPreference) -> Self {
        self.interval_preference = preference;
        self
    }

    pub fn fastest_interval(mut self) -> Self {
        self.interval_preference = CameraIntervalPreference::Fastest;
        self
    }

    pub fn slowest_interval(mut self) -> Self {
        self.interval_preference = CameraIntervalPreference::Slowest;
        self
    }

    pub fn default_interval(mut self) -> Self {
        self.interval_preference = CameraIntervalPreference::Default;
        self
    }

    pub fn exact_interval(mut self, interval: Interval) -> Self {
        self.interval_preference = CameraIntervalPreference::Exact(interval);
        self
    }

    pub fn no_interval(mut self) -> Self {
        self.interval_preference = CameraIntervalPreference::None;
        self
    }

    pub fn control(mut self, id: ControlId, value: ControlValue) -> Self {
        self.controls.push((id, value));
        self
    }

    pub fn enable_tdn_output(mut self, enable: bool) -> Self {
        self.tdn_output_mode = if enable {
            TdnOutputMode::Force
        } else {
            TdnOutputMode::Off
        };
        self
    }

    pub fn tdn_output_mode(mut self, mode: TdnOutputMode) -> Self {
        self.tdn_output_mode = mode;
        self
    }

    pub fn select(&self) -> Result<SelectedCamera, CaptureError> {
        self.select_all()?
            .into_iter()
            .next()
            .ok_or(CaptureError::NoCameraMatchingRequest)
    }

    pub fn select_all(&self) -> Result<Vec<SelectedCamera>, CaptureError> {
        let mut candidates = self.candidates();
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.score()));
        let mut seen_devices = std::collections::HashSet::new();
        let mut selected = Vec::new();
        for candidate in candidates {
            if !seen_devices.insert(candidate.device_index) {
                continue;
            }
            selected.push(SelectedCamera {
                device: self.devices[candidate.device_index].clone(),
                backend: candidate.backend,
                mode: candidate.mode,
                interval: candidate.interval,
                controls: self.controls.clone(),
                tdn_output_mode: self.tdn_output_mode,
            });
        }
        if selected.is_empty() {
            Err(CaptureError::NoCameraMatchingRequest)
        } else {
            Ok(selected)
        }
    }

    pub fn select_many(&self, count: usize) -> Result<Vec<SelectedCamera>, CaptureError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let selected: Vec<_> = self.select_all()?.into_iter().take(count).collect();
        if selected.is_empty() {
            Err(CaptureError::NoCameraMatchingRequest)
        } else {
            Ok(selected)
        }
    }

    pub fn start(self) -> Result<super::handle::CaptureHandle, CaptureError> {
        self.start_with_policy(CaptureStartPolicy::default())
    }

    pub fn start_with_policy(
        self,
        policy: CaptureStartPolicy,
    ) -> Result<super::handle::CaptureHandle, CaptureError> {
        self.select()?.start_with_policy(policy)
    }

    pub fn start_all(self) -> Result<Vec<super::handle::CaptureHandle>, CaptureError> {
        self.start_all_with_policy(CaptureStartPolicy::default())
    }

    pub fn start_many(
        self,
        count: usize,
    ) -> Result<Vec<super::handle::CaptureHandle>, CaptureError> {
        self.start_many_with_policy(count, CaptureStartPolicy::default())
    }

    pub fn start_all_with_policy(
        self,
        policy: CaptureStartPolicy,
    ) -> Result<Vec<super::handle::CaptureHandle>, CaptureError> {
        self.select_all()?
            .into_iter()
            .map(|selected| selected.start_with_policy(policy))
            .collect()
    }

    pub fn start_many_with_policy(
        self,
        count: usize,
        policy: CaptureStartPolicy,
    ) -> Result<Vec<super::handle::CaptureHandle>, CaptureError> {
        self.select_many(count)?
            .into_iter()
            .map(|selected| selected.start_with_policy(policy))
            .collect()
    }

    fn candidates(&self) -> Vec<CameraCandidate> {
        let mut candidates = Vec::new();
        for (device_index, device) in self.devices.iter().enumerate() {
            for backend in &device.backends {
                let Some(backend_priority) = self.backend_rank(backend.kind) else {
                    continue;
                };
                for mode in &backend.descriptor.modes {
                    let Some(format_priority) = self.format_rank(mode.format.code) else {
                        continue;
                    };
                    let Some(interval) = self.interval_for_mode(mode) else {
                        continue;
                    };
                    let width = mode.format.resolution.width.get();
                    let height = mode.format.resolution.height.get();
                    let Some(resolution_priority) = self.resolution_rank(width, height) else {
                        continue;
                    };
                    let fits_requested_size = self.min_width.is_none_or(|min| width >= min)
                        && self.min_height.is_none_or(|min| height >= min)
                        && self.max_width.is_none_or(|max| width <= max)
                        && self.max_height.is_none_or(|max| height <= max);
                    if !fits_requested_size {
                        continue;
                    }
                    let area = width as u64 * height as u64;
                    let fps_milli = interval.map(interval_fps_milli).unwrap_or(0);
                    let candidate = CameraCandidate {
                        device_index,
                        backend: backend.kind,
                        mode: mode.id.clone(),
                        interval,
                        backend_priority,
                        format_priority,
                        resolution_priority,
                        fps_milli,
                        area,
                    };
                    candidates.push(candidate);
                }
            }
        }
        candidates
    }

    fn backend_rank(&self, backend: BackendKind) -> Option<u8> {
        self.backend_priority
            .iter()
            .position(|candidate| *candidate == backend)
            .map(|index| (self.backend_priority.len().saturating_sub(index)) as u8)
    }

    fn format_rank(&self, code: FourCc) -> Option<u8> {
        self.format_priority
            .iter()
            .position(|candidate| *candidate == code)
            .map(|index| (self.format_priority.len().saturating_sub(index)) as u8)
    }

    fn resolution_rank(&self, width: u32, height: u32) -> Option<u8> {
        if self.resolution_priority.is_empty() {
            return Some(1);
        }
        self.resolution_priority
            .iter()
            .position(|candidate| *candidate == (width, height))
            .map(|index| (self.resolution_priority.len().saturating_sub(index)) as u8)
    }

    fn interval_for_mode(&self, mode: &Mode) -> Option<Option<Interval>> {
        match self.interval_preference {
            CameraIntervalPreference::Default => Some(default_interval(mode)),
            CameraIntervalPreference::Fastest => Some(fastest_interval(mode)),
            CameraIntervalPreference::Slowest => Some(slowest_interval(mode)),
            CameraIntervalPreference::Exact(interval) => {
                if mode.intervals.is_empty() || mode.intervals.contains(&interval) {
                    Some(Some(interval))
                } else {
                    None
                }
            }
            CameraIntervalPreference::None => Some(None),
        }
    }
}

impl Default for CameraRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct SelectedCamera {
    pub device: ProbedDevice,
    pub backend: BackendKind,
    pub mode: ModeId,
    pub interval: Option<Interval>,
    pub controls: Vec<(ControlId, ControlValue)>,
    pub tdn_output_mode: TdnOutputMode,
}

impl SelectedCamera {
    pub fn capture_request(&self) -> CaptureRequest<'_> {
        let mut request = CaptureRequest::new(&self.device)
            .backend(self.backend)
            .mode(self.mode.clone())
            .tdn_output_mode(self.tdn_output_mode);
        if let Some(interval) = self.interval {
            request = request.interval(interval);
        }
        for (id, value) in &self.controls {
            request = request.control(*id, value.clone());
        }
        request
    }

    pub fn start(&self) -> Result<super::handle::CaptureHandle, CaptureError> {
        self.capture_request().start()
    }

    pub fn start_with_policy(
        &self,
        policy: CaptureStartPolicy,
    ) -> Result<super::handle::CaptureHandle, CaptureError> {
        self.capture_request().start_with_policy(policy)
    }
}

#[derive(Debug, Clone)]
struct CameraCandidate {
    device_index: usize,
    backend: BackendKind,
    mode: ModeId,
    interval: Option<Interval>,
    backend_priority: u8,
    format_priority: u8,
    resolution_priority: u8,
    fps_milli: u64,
    area: u64,
}

impl CameraCandidate {
    fn score(&self) -> (u8, u8, u8, u64, u64) {
        (
            self.format_priority,
            self.resolution_priority,
            self.backend_priority,
            self.fps_milli,
            self.area,
        )
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn classified_control_apply_requests_control_drop() {
        let err = CaptureError::classified_control_apply(
            ControlApplyKind::InvalidArgument,
            "invalid argument",
        );
        assert!(err.requires_dropping_controls());
        assert!(!err.requires_disabling_tdn());
    }

    #[test]
    fn libcamera_tdn_unavailable_requests_tdn_disable_and_retry() {
        let err = CaptureError::LibcameraTdnOutputUnavailable;
        assert!(err.requires_disabling_tdn());
        assert!(err.is_transient_start());
        assert!(err.retryable());
    }

    #[test]
    fn generic_control_apply_is_not_treated_as_control_drop() {
        let err = CaptureError::control_apply("channel closed");
        assert!(!err.requires_dropping_controls());
    }
}

/// Builder for starting capture with backend/mode/controls validated ahead of time.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let device = probe_all().into_iter().next().expect("device");
/// let handle = CaptureRequest::new(&device)
///     .backend_preferred(Some(BackendKind::V4l2))
///     .start()?;
/// let _ = handle.recv();
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
#[derive(Debug, Clone)]
pub struct CaptureRequest<'a> {
    device: &'a ProbedDevice,
    backend: Option<BackendKind>,
    mode: Option<ModeId>,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    tdn_output_mode: TdnOutputMode,
}

impl<'a> CaptureRequest<'a> {
    /// Create a new request targeting a probed device.
    pub fn new(device: &'a ProbedDevice) -> Self {
        Self {
            device,
            backend: None,
            mode: None,
            interval: None,
            controls: Vec::new(),
            tdn_output_mode: TdnOutputMode::default(),
        }
    }

    /// Pin to a backend kind.
    ///
    /// If the backend is missing/unavailable, `start` returns an error.
    pub fn backend(mut self, kind: BackendKind) -> Self {
        self.backend = Some(kind);
        self
    }

    /// Apply defaults for an optional preferred backend.
    ///
    /// Pass `None` to select the first available backend.
    pub fn backend_preferred(mut self, kind: Option<BackendKind>) -> Self {
        self.backend = kind;
        self
    }

    /// Pin to a specific mode id.
    ///
    /// Use the `ModeId` from a probed backend descriptor.
    pub fn mode(mut self, mode: ModeId) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Pin to a specific interval (must exist in the chosen mode).
    ///
    /// If a backend does not advertise intervals, validation is relaxed.
    pub fn interval(mut self, interval: Interval) -> Self {
        self.interval = Some(interval);
        self
    }

    /// Queue a control assignment to apply before streaming.
    pub fn control(mut self, id: ControlId, value: ControlValue) -> Self {
        self.controls.push((id, value));
        self
    }

    /// Request a dedicated TDN output stream (libcamera PiSP).
    ///
    /// Requires the `libcamera` backend and hardware support.
    pub fn enable_tdn_output(mut self, enable: bool) -> Self {
        self.tdn_output_mode = if enable {
            TdnOutputMode::Force
        } else {
            TdnOutputMode::Off
        };
        self
    }

    /// Configure how (or if) a TDN output stream is requested.
    pub fn tdn_output_mode(mut self, mode: TdnOutputMode) -> Self {
        self.tdn_output_mode = mode;
        self
    }

    /// Resolve the canonical backend descriptor that this request will use.
    pub fn resolved_descriptor(&self) -> Result<CaptureDescriptor, CaptureError> {
        let (_, mode, descriptor) = self.resolve_backend_mode()?;
        Ok(minimize_capture_descriptor(&descriptor, &mode.id))
    }

    /// Start capture after validating backend/mode/interval/controls.
    ///
    /// Returns a running `CaptureHandle` that can receive frames.
    pub fn start(self) -> Result<super::handle::CaptureHandle, CaptureError> {
        self.start_with_policy(CaptureStartPolicy::default())
    }

    /// Start capture using Styx-owned retry and fallback behavior.
    pub fn start_with_policy(
        mut self,
        policy: CaptureStartPolicy,
    ) -> Result<super::handle::CaptureHandle, CaptureError> {
        let attempts = policy.max_attempts.max(1);
        for attempt in 0..attempts {
            let (backend, mode, descriptor) = self.resolve_backend_mode()?;
            let interval = self.interval.or_else(|| default_interval(&mode));
            match start_backend(
                backend,
                mode,
                interval,
                descriptor,
                self.controls.clone(),
                self.tdn_output_mode,
            ) {
                Ok(handle) => return Ok(handle),
                Err(err) => {
                    if policy.retry_with_tdn_disabled
                        && self.try_disable_noise_reduction(backend.kind, &err)
                    {
                        sleep_before_retry(policy.retry_backoff);
                        continue;
                    }
                    if policy.retry_without_controls_on_control_errors
                        && self.try_drop_controls(backend.kind, &err)
                    {
                        sleep_before_retry(policy.retry_backoff);
                        continue;
                    }
                    if policy.retry_transient_errors
                        && err.is_transient_start()
                        && attempt + 1 < attempts
                    {
                        sleep_before_retry(policy.retry_backoff);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        Err(CaptureError::Backend(
            "capture start policy exhausted without returning a result".into(),
        ))
    }

    fn resolve_backend_mode(
        &self,
    ) -> Result<(&'a ProbedBackend, Mode, CaptureDescriptor), CaptureError> {
        let backend = pick_backend(self.device, self.backend)?;
        let mode = pick_mode(backend, self.mode.clone())?.clone();
        validate_config(backend, &mode, self.interval, &self.controls)?;
        Ok((backend, mode, backend.descriptor.clone()))
    }

    fn try_disable_noise_reduction(&mut self, backend: BackendKind, err: &CaptureError) -> bool {
        const NOISE_REDUCTION_MODE: u32 = 10002;
        if backend != BackendKind::Libcamera || !err.requires_disabling_tdn() {
            return false;
        }

        let mut updated = false;
        for (id, value) in &mut self.controls {
            if id.0 == NOISE_REDUCTION_MODE {
                if !matches!(value, ControlValue::Int(0)) {
                    *value = ControlValue::Int(0);
                    updated = true;
                }
                self.tdn_output_mode = TdnOutputMode::Off;
                return updated;
            }
        }

        self.controls
            .push((ControlId(NOISE_REDUCTION_MODE), ControlValue::Int(0)));
        self.tdn_output_mode = TdnOutputMode::Off;
        true
    }

    fn try_drop_controls(&mut self, backend: BackendKind, err: &CaptureError) -> bool {
        if backend != BackendKind::Libcamera
            || self.controls.is_empty()
            || !err.requires_dropping_controls()
        {
            return false;
        }

        self.controls.clear();
        self.tdn_output_mode = TdnOutputMode::Off;
        true
    }
}

/// Start capture on the preferred backend (or first available), returning a handle.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let device = probe_all().into_iter().next().expect("device");
/// let handle = start_capture(&device, None)?;
/// let _ = handle.recv();
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
pub fn start_capture(
    device: &ProbedDevice,
    preferred: Option<BackendKind>,
) -> Result<super::handle::CaptureHandle, CaptureError> {
    CaptureRequest::new(device)
        .backend_preferred(preferred)
        .start()
}

fn fastest_interval(mode: &Mode) -> Option<Interval> {
    mode.intervals
        .iter()
        .copied()
        .max_by(|a, b| interval_fps_milli(*a).cmp(&interval_fps_milli(*b)))
        .or_else(|| mode.interval_stepwise.map(|s| s.min))
}

fn slowest_interval(mode: &Mode) -> Option<Interval> {
    mode.intervals
        .iter()
        .copied()
        .min_by(|a, b| interval_fps_milli(*a).cmp(&interval_fps_milli(*b)))
        .or_else(|| mode.interval_stepwise.map(|s| s.max))
}

fn interval_fps_milli(interval: Interval) -> u64 {
    (interval.denominator.get() as u64)
        .saturating_mul(1_000)
        .checked_div(interval.numerator.get().max(1) as u64)
        .unwrap_or(0)
}

fn pick_backend(
    device: &ProbedDevice,
    preferred: Option<BackendKind>,
) -> Result<&ProbedBackend, CaptureError> {
    if device.backends.is_empty() {
        return Err(CaptureError::NoBackend);
    }
    if let Some(kind) = preferred {
        device
            .backends
            .iter()
            .find(|b| b.kind == kind)
            .ok_or(CaptureError::BackendUnavailable(kind))
    } else {
        Ok(&device.backends[0])
    }
}

fn pick_mode(backend: &ProbedBackend, mode: Option<ModeId>) -> Result<&Mode, CaptureError> {
    if backend.descriptor.modes.is_empty() {
        return Err(CaptureError::NoModes);
    }
    if let Some(id) = mode {
        let requested = &id.format;
        let is_bayer = requested.code == FourCc::new(*b"RGGB")
            || requested.code == FourCc::new(*b"BGGR")
            || requested.code == FourCc::new(*b"GBRG")
            || requested.code == FourCc::new(*b"GRBG");

        // Prefer an exact ModeId match, then exact MediaFormat matches.
        if let Some(found) = backend.descriptor.modes.iter().find(|m| m.id == id) {
            return Ok(found);
        }
        if let Some(found) = backend
            .descriptor
            .modes
            .iter()
            .find(|m| m.id.format == *requested || m.format == *requested)
        {
            return Ok(found);
        }

        // Fall back to matching by code+resolution, relaxing color-space when either side is
        // Unknown (or for raw Bayer formats where color-space is not a meaningful selector).
        backend
            .descriptor
            .modes
            .iter()
            .find(|m| {
                let advertised_id = &m.id.format;
                let advertised_format = &m.format;

                let matches_id = advertised_id.code == requested.code
                    && advertised_id.resolution == requested.resolution;
                let matches_format = advertised_format.code == requested.code
                    && advertised_format.resolution == requested.resolution;
                if !matches_id && !matches_format {
                    return false;
                }

                let advertised_color = if matches_id {
                    advertised_id.color
                } else {
                    advertised_format.color
                };
                advertised_color == requested.color
                    || advertised_color == ColorSpace::Unknown
                    || requested.color == ColorSpace::Unknown
                    || is_bayer
            })
            .ok_or(CaptureError::InvalidMode(id))
    } else {
        Ok(&backend.descriptor.modes[0])
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::BackendHandle;

    #[test]
    fn pick_mode_ignores_color_when_unknown() {
        let fmt_advertised = MediaFormat::new(
            FourCc::new(*b"RGGB"),
            Resolution::new(1280, 800).unwrap(),
            ColorSpace::Unknown,
        );
        let fmt_requested = MediaFormat::new(
            FourCc::new(*b"RGGB"),
            Resolution::new(1280, 800).unwrap(),
            ColorSpace::Bt709,
        );
        let advertised_mode = Mode {
            id: ModeId {
                format: fmt_advertised,
                interval: None,
            },
            format: fmt_advertised,
            intervals: smallvec::smallvec![],
            interval_stepwise: None,
        };
        let backend = ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes: vec![advertised_mode.clone()],
                controls: vec![],
            },
            properties: vec![],
        };

        let requested_id = ModeId {
            format: fmt_requested,
            interval: None,
        };
        let picked = pick_mode(&backend, Some(requested_id)).expect("pick");
        assert_eq!(picked.id.format.code, FourCc::new(*b"RGGB"));
        assert_eq!(picked.id.format.resolution.width.get(), 1280);
        assert_eq!(picked.id.format.resolution.height.get(), 800);
    }

    #[test]
    fn pick_mode_accepts_mode_format_when_id_format_differs() {
        let fmt_id = MediaFormat::new(
            FourCc::new(*b"RGGB"),
            Resolution::new(1280, 800).unwrap(),
            ColorSpace::Unknown,
        );
        let fmt_mode = MediaFormat::new(
            FourCc::new(*b"RGGB"),
            Resolution::new(1280, 800).unwrap(),
            ColorSpace::Srgb,
        );
        let advertised_mode = Mode {
            id: ModeId {
                format: fmt_id,
                interval: None,
            },
            format: fmt_mode,
            intervals: smallvec::smallvec![],
            interval_stepwise: None,
        };
        let backend = ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes: vec![advertised_mode.clone()],
                controls: vec![],
            },
            properties: vec![],
        };

        let requested = ModeId {
            format: fmt_mode,
            interval: None,
        };
        let picked = pick_mode(&backend, Some(requested)).expect("pick");
        assert_eq!(picked.format.color, ColorSpace::Srgb);
    }

    #[test]
    fn pick_mode_relaxes_color_for_bayer() {
        let fmt_advertised = MediaFormat::new(
            FourCc::new(*b"RGGB"),
            Resolution::new(1280, 800).unwrap(),
            ColorSpace::Bt709,
        );
        let fmt_requested = MediaFormat::new(
            FourCc::new(*b"RGGB"),
            Resolution::new(1280, 800).unwrap(),
            ColorSpace::Srgb,
        );
        let advertised_mode = Mode {
            id: ModeId {
                format: fmt_advertised,
                interval: None,
            },
            format: fmt_advertised,
            intervals: smallvec::smallvec![],
            interval_stepwise: None,
        };
        let backend = ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes: vec![advertised_mode.clone()],
                controls: vec![],
            },
            properties: vec![],
        };

        let requested_id = ModeId {
            format: fmt_requested,
            interval: None,
        };
        let picked = pick_mode(&backend, Some(requested_id)).expect("pick");
        assert_eq!(picked.id.format.code, FourCc::new(*b"RGGB"));
    }

    #[test]
    fn resolved_descriptor_returns_only_selected_mode() {
        let fmt_primary = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(640, 480).unwrap(),
            ColorSpace::Srgb,
        );
        let fmt_secondary = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(1280, 720).unwrap(),
            ColorSpace::Srgb,
        );
        let requested_mode = Mode {
            id: ModeId {
                format: fmt_secondary,
                interval: None,
            },
            format: fmt_secondary,
            intervals: smallvec::smallvec![],
            interval_stepwise: None,
        };
        let backend = ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes: vec![
                    Mode {
                        id: ModeId {
                            format: fmt_primary,
                            interval: None,
                        },
                        format: fmt_primary,
                        intervals: smallvec::smallvec![],
                        interval_stepwise: None,
                    },
                    requested_mode.clone(),
                ],
                controls: vec![],
            },
            properties: vec![],
        };
        let device = ProbedDevice {
            identity: crate::DeviceIdentity {
                display: "virtual".to_string(),
                keys: vec!["virtual".to_string()],
            },
            backends: vec![backend],
        };

        let descriptor = CaptureRequest::new(&device)
            .backend(BackendKind::Virtual)
            .mode(requested_mode.id.clone())
            .resolved_descriptor()
            .expect("resolve descriptor");

        assert_eq!(descriptor.modes.len(), 1);
        assert_eq!(descriptor.modes[0].id, requested_mode.id);
    }

    #[test]
    fn camera_request_selects_best_matching_mode() {
        let small = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(640, 480).unwrap(),
            ColorSpace::Srgb,
        );
        let large = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(1920, 1080).unwrap(),
            ColorSpace::Srgb,
        );
        let slow = Interval {
            numerator: std::num::NonZeroU32::new(1).unwrap(),
            denominator: std::num::NonZeroU32::new(30).unwrap(),
        };
        let fast = Interval {
            numerator: std::num::NonZeroU32::new(1).unwrap(),
            denominator: std::num::NonZeroU32::new(60).unwrap(),
        };
        let modes = vec![
            Mode {
                id: ModeId {
                    format: large,
                    interval: Some(slow),
                },
                format: large,
                intervals: smallvec::smallvec![slow],
                interval_stepwise: None,
            },
            Mode {
                id: ModeId {
                    format: small,
                    interval: Some(fast),
                },
                format: small,
                intervals: smallvec::smallvec![slow, fast],
                interval_stepwise: None,
            },
        ];
        let device = ProbedDevice {
            identity: crate::DeviceIdentity {
                display: "virtual".to_string(),
                keys: vec!["virtual".to_string()],
            },
            backends: vec![ProbedBackend {
                kind: BackendKind::Virtual,
                handle: BackendHandle::Virtual,
                descriptor: CaptureDescriptor {
                    modes,
                    controls: vec![],
                },
                properties: vec![],
            }],
        };

        let selected = CameraRequest::from_devices(vec![device])
            .max_resolution(640, 480)
            .select()
            .expect("camera selection");

        assert_eq!(selected.backend, BackendKind::Virtual);
        assert_eq!(selected.mode.format.resolution, small.resolution);
        assert_eq!(selected.interval, Some(fast));
    }

    #[test]
    fn camera_request_uses_backend_priority() {
        let fmt = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(640, 480).unwrap(),
            ColorSpace::Srgb,
        );
        let mode = Mode {
            id: ModeId {
                format: fmt,
                interval: None,
            },
            format: fmt,
            intervals: smallvec::smallvec![],
            interval_stepwise: None,
        };
        let device = ProbedDevice {
            identity: crate::DeviceIdentity {
                display: "dual".to_string(),
                keys: vec!["dual".to_string()],
            },
            backends: vec![
                ProbedBackend {
                    kind: BackendKind::Libcamera,
                    handle: BackendHandle::Virtual,
                    descriptor: CaptureDescriptor {
                        modes: vec![mode.clone()],
                        controls: vec![],
                    },
                    properties: vec![],
                },
                ProbedBackend {
                    kind: BackendKind::V4l2,
                    handle: BackendHandle::Virtual,
                    descriptor: CaptureDescriptor {
                        modes: vec![mode],
                        controls: vec![],
                    },
                    properties: vec![],
                },
            ],
        };

        let selected = CameraRequest::from_devices(vec![device])
            .backend_priority([BackendKind::Libcamera, BackendKind::V4l2])
            .select()
            .expect("camera selection");

        assert_eq!(selected.backend, BackendKind::Libcamera);
    }

    #[test]
    fn camera_request_accepts_string_format_priority_and_selects_all() {
        let rg24 = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(640, 480).unwrap(),
            ColorSpace::Srgb,
        );
        let nv12 = MediaFormat::new(
            FourCc::new(*b"NV12"),
            Resolution::new(640, 480).unwrap(),
            ColorSpace::Srgb,
        );
        let make_backend = |kind, format| ProbedBackend {
            kind,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes: vec![Mode {
                    id: ModeId {
                        format,
                        interval: None,
                    },
                    format,
                    intervals: smallvec::smallvec![],
                    interval_stepwise: None,
                }],
                controls: vec![],
            },
            properties: vec![],
        };
        let first_device = ProbedDevice {
            identity: crate::DeviceIdentity {
                display: "formats-a".to_string(),
                keys: vec!["formats-a".to_string()],
            },
            backends: vec![
                make_backend(BackendKind::Virtual, rg24),
                make_backend(BackendKind::File, nv12),
            ],
        };
        let second_device = ProbedDevice {
            identity: crate::DeviceIdentity {
                display: "formats-b".to_string(),
                keys: vec!["formats-b".to_string()],
            },
            backends: vec![make_backend(BackendKind::Virtual, rg24)],
        };

        let selected = CameraRequest::from_devices(vec![first_device, second_device])
            .backend_priority([BackendKind::Virtual, BackendKind::File])
            .format_priority(["NV12", "RG24"])
            .min_resolution(320, 240)
            .select_many(2)
            .expect("camera selection");

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].mode.format.code, FourCc::new(*b"NV12"));
    }

    #[test]
    fn camera_request_filters_and_ranks_resolution_priority() {
        let small = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(640, 480).unwrap(),
            ColorSpace::Srgb,
        );
        let preferred = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(1280, 720).unwrap(),
            ColorSpace::Srgb,
        );
        let fallback = MediaFormat::new(
            FourCc::new(*b"RG24"),
            Resolution::new(1920, 1080).unwrap(),
            ColorSpace::Srgb,
        );
        let modes = [small, fallback, preferred]
            .into_iter()
            .map(|format| Mode {
                id: ModeId {
                    format,
                    interval: None,
                },
                format,
                intervals: smallvec::smallvec![],
                interval_stepwise: None,
            })
            .collect();
        let device = ProbedDevice {
            identity: crate::DeviceIdentity {
                display: "resolutions".to_string(),
                keys: vec!["resolutions".to_string()],
            },
            backends: vec![ProbedBackend {
                kind: BackendKind::Virtual,
                handle: BackendHandle::Virtual,
                descriptor: CaptureDescriptor {
                    modes,
                    controls: vec![],
                },
                properties: vec![],
            }],
        };

        let selected = CameraRequest::from_devices(vec![device])
            .resolution_priority([(1280, 720), (1920, 1080)])
            .select()
            .expect("camera selection");

        assert_eq!(selected.mode.format.resolution, preferred.resolution);
    }
}

fn default_interval(mode: &Mode) -> Option<Interval> {
    mode.intervals
        .first()
        .copied()
        .or_else(|| mode.interval_stepwise.map(|s| s.min))
}

fn minimize_capture_descriptor(
    descriptor: &CaptureDescriptor,
    selected_mode: &ModeId,
) -> CaptureDescriptor {
    let controls = descriptor.controls.clone();
    let modes = descriptor
        .modes
        .iter()
        .find(|mode| &mode.id == selected_mode)
        .cloned()
        .into_iter()
        .collect();
    CaptureDescriptor { modes, controls }
}

fn sleep_before_retry(delay: Duration) {
    if !delay.is_zero() {
        std::thread::sleep(delay);
    }
}

fn validate_config(
    backend: &ProbedBackend,
    mode: &Mode,
    interval: Option<Interval>,
    controls: &[(ControlId, ControlValue)],
) -> Result<(), CaptureError> {
    // Some backends (notably libcamera) do not provide enumerated interval lists even though they
    // can honor a requested frame duration via controls. When a mode advertises no intervals and
    // no stepwise descriptor, treat interval pinning as "best effort" and validate everything
    // else against the descriptor.
    let interval_for_validation =
        if interval.is_some() && mode.intervals.is_empty() && mode.interval_stepwise.is_none() {
            None
        } else {
            interval
        };
    let cfg = CaptureConfig {
        mode: mode.id.clone(),
        interval: interval_for_validation,
        controls: controls.to_vec(),
    };
    cfg.validate(&backend.descriptor)
        .map_err(CaptureError::InvalidConfig)
}
