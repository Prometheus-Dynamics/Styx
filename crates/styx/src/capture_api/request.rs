use crate::{BackendKind, ProbedBackend, ProbedDevice};
use std::time::Duration;
use styx_capture::prelude::*;

#[cfg(feature = "file-backend")]
use super::FileSourceConfig;
#[cfg(feature = "libcamera")]
use super::LIBCAMERA_NOISE_REDUCTION_MODE;
#[cfg(feature = "netcam")]
use super::NetcamSourceConfig;
use super::VirtualSourceConfig;
use super::dispatch::start_backend;
use super::tunables::StyxConfig;
use crate::metrics::CaptureRetryMetrics;

mod camera;
mod error;
mod source;
pub use camera::{
    CameraFormat, CameraIntervalPreference, CameraRequest, CameraStartPolicy, SelectedCamera,
};
pub use error::{CaptureError, ControlApplyKind};
pub use source::CaptureSource;

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

/// Builder for starting capture with backend/mode/controls validated ahead of time.
///
/// # Example
/// ```rust,no_run
/// use styx::prelude::*;
///
/// let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
/// let handle = CaptureRequest::new(&device)
///     .backend_preferred(Some(BackendKind::Virtual))
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
    config: Option<StyxConfig>,
}

impl<'a> CaptureRequest<'a> {
    pub fn source(device: ProbedDevice) -> CaptureSource {
        CaptureSource::new(device)
    }

    pub fn virtual_source(config: VirtualSourceConfig) -> CaptureSource {
        CaptureSource::new(config.into_device())
    }

    #[cfg(feature = "netcam")]
    pub fn netcam_source(config: NetcamSourceConfig) -> CaptureSource {
        CaptureSource::new(config.into_device())
    }

    #[cfg(feature = "file-backend")]
    pub fn file_source(config: FileSourceConfig) -> CaptureSource {
        CaptureSource::new(config.into_device())
    }

    /// Play a `.styxrec` recording back as a camera (see [`crate::replay`]).
    pub fn replay_source(
        config: crate::replay::ReplaySourceConfig,
    ) -> Result<CaptureSource, crate::replay::ReplayError> {
        Ok(CaptureSource::new(config.into_device()?))
    }

    /// Create a new request targeting a probed device.
    pub fn new(device: &'a ProbedDevice) -> Self {
        Self {
            device,
            backend: None,
            mode: None,
            interval: None,
            controls: Vec::new(),
            tdn_output_mode: TdnOutputMode::default(),
            config: None,
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

    /// Attach a hardware-scaled GREY/YUV pyramid companion at `2^-level` resolution when the
    /// backend can produce one (libcamera ISPs with a second output, a native camera's PiSP).
    /// See [`StyxConfig::libcamera_pyramid_level`] and [`StyxConfig::native_pyramid_level`].
    /// Call after [`CaptureRequest::config`], which replaces the whole configuration.
    pub fn luma_pyramid(mut self, level: u8) -> Self {
        let config = self.config.take().unwrap_or_default();
        self.config = Some(
            config
                .libcamera_pyramid_level(level)
                .native_pyramid_level(level),
        );
        self
    }

    /// Use request-local runtime tunables instead of the defaults.
    pub fn config(mut self, config: StyxConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Resolve the canonical backend descriptor that this request will use.
    pub fn resolved_descriptor(&self) -> Result<CaptureDescriptor, CaptureError> {
        let (_, mode, descriptor) = self.resolve_backend_mode()?;
        Ok(minimize_capture_descriptor(&descriptor, &mode.id))
    }

    /// Start the backend capture feeding `queue`, without retries or supervision (used by the
    /// reconnect supervisor).
    pub(crate) fn start_into(
        self,
        queue: super::handle::CaptureQueue,
    ) -> Result<super::handle::CaptureHandle, CaptureError> {
        let (backend, mode, descriptor) = self.resolve_backend_mode()?;
        let interval = self.interval.or_else(|| default_interval(&mode));
        let config = self.config.clone().unwrap_or_default();
        start_backend(
            backend,
            mode,
            interval,
            descriptor,
            self.controls.clone(),
            self.tdn_output_mode,
            &config,
            Some(queue),
        )
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
        let retry_metrics = CaptureRetryMetrics::default();
        for attempt in 0..attempts {
            let (backend, mode, descriptor) = self.resolve_backend_mode()?;
            let interval = self.interval.or_else(|| default_interval(&mode));
            let config = self.config.clone().unwrap_or_default();
            config.apply_runtime_tunables();
            let mode_id = mode.id.clone();
            let queue = super::supervisor::queue_for(backend.kind, &config);
            match start_backend(
                backend,
                mode,
                interval,
                descriptor,
                self.controls.clone(),
                self.tdn_output_mode,
                &config,
                queue
                    .as_ref()
                    .map(|q| super::supervisor::backend_queue(&q.0)),
            ) {
                Ok(handle) => {
                    handle
                        .retry_metrics
                        .merge_snapshot(retry_metrics.snapshot());
                    let name = self.device.identity.display.clone();
                    return Ok(match queue {
                        Some(queue) => super::supervisor::supervise(
                            handle,
                            queue,
                            self.device,
                            mode_id,
                            interval,
                            self.controls.clone(),
                            self.tdn_output_mode,
                            config,
                        ),
                        None => handle,
                    }
                    .published(&name));
                }
                Err(err) => {
                    if policy.retry_with_tdn_disabled
                        && self.try_disable_noise_reduction(backend.kind, &err)
                    {
                        retry_metrics.record_start_retry("disable_tdn", err.to_string());
                        log_start_retry(backend.kind, attempt + 1, attempts, "disable_tdn", &err);
                        sleep_before_retry(policy.retry_backoff);
                        continue;
                    }
                    if policy.retry_without_controls_on_control_errors
                        && self.try_drop_controls(backend.kind, &err)
                    {
                        retry_metrics.record_start_retry("drop_controls", err.to_string());
                        log_start_retry(backend.kind, attempt + 1, attempts, "drop_controls", &err);
                        sleep_before_retry(policy.retry_backoff);
                        continue;
                    }
                    if policy.retry_transient_errors
                        && err.is_transient_start()
                        && attempt + 1 < attempts
                    {
                        retry_metrics.record_start_retry("transient_retry", err.to_string());
                        log_start_retry(
                            backend.kind,
                            attempt + 1,
                            attempts,
                            "transient_retry",
                            &err,
                        );
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

    /// Start capture using retry and fallback behavior without blocking Tokio during retry backoff.
    ///
    /// Backend startup itself is still synchronous because camera APIs are sync-first. This async
    /// variant exists so resilient retry sleeps yield the runtime instead of parking a Tokio worker.
    /// If startup latency matters in an async service, run request construction and `start_with_policy`
    /// from an application-owned blocking thread/task, then move the returned handle back into async
    /// code for `recv_async` or pipeline worker use.
    #[cfg(feature = "async")]
    pub async fn start_with_policy_async(
        mut self,
        policy: CaptureStartPolicy,
    ) -> Result<super::handle::CaptureHandle, CaptureError> {
        let attempts = policy.max_attempts.max(1);
        let retry_metrics = CaptureRetryMetrics::default();
        for attempt in 0..attempts {
            let (backend, mode, descriptor) = self.resolve_backend_mode()?;
            let backend_kind = backend.kind;
            let backend = backend.clone();
            let interval = self.interval.or_else(|| default_interval(&mode));
            let config = self.config.clone().unwrap_or_default();
            config.apply_runtime_tunables();
            let controls = self.controls.clone();
            let tdn_output_mode = self.tdn_output_mode;
            let mode_id = mode.id.clone();
            let queue = super::supervisor::queue_for(backend_kind, &config);
            let backend_queue = queue
                .as_ref()
                .map(|q| super::supervisor::backend_queue(&q.0));
            let supervise_config = config.clone();
            let started = tokio::task::spawn_blocking(move || {
                start_backend(
                    &backend,
                    mode,
                    interval,
                    descriptor,
                    controls,
                    tdn_output_mode,
                    &config,
                    backend_queue,
                )
            })
            .await
            .map_err(|err| CaptureError::Backend(format!("capture startup task failed: {err}")))?;
            match started {
                Ok(handle) => {
                    handle
                        .retry_metrics
                        .merge_snapshot(retry_metrics.snapshot());
                    let name = self.device.identity.display.clone();
                    return Ok(match queue {
                        Some(queue) => super::supervisor::supervise(
                            handle,
                            queue,
                            self.device,
                            mode_id,
                            interval,
                            self.controls.clone(),
                            self.tdn_output_mode,
                            supervise_config,
                        ),
                        None => handle,
                    }
                    .published(&name));
                }
                Err(err) => {
                    if policy.retry_with_tdn_disabled
                        && self.try_disable_noise_reduction(backend_kind, &err)
                    {
                        retry_metrics.record_start_retry("disable_tdn", err.to_string());
                        log_start_retry(backend_kind, attempt + 1, attempts, "disable_tdn", &err);
                        sleep_before_retry_async(policy.retry_backoff).await;
                        continue;
                    }
                    if policy.retry_without_controls_on_control_errors
                        && self.try_drop_controls(backend_kind, &err)
                    {
                        retry_metrics.record_start_retry("drop_controls", err.to_string());
                        log_start_retry(backend_kind, attempt + 1, attempts, "drop_controls", &err);
                        sleep_before_retry_async(policy.retry_backoff).await;
                        continue;
                    }
                    if policy.retry_transient_errors
                        && err.is_transient_start()
                        && attempt + 1 < attempts
                    {
                        retry_metrics.record_start_retry("transient_retry", err.to_string());
                        log_start_retry(
                            backend_kind,
                            attempt + 1,
                            attempts,
                            "transient_retry",
                            &err,
                        );
                        sleep_before_retry_async(policy.retry_backoff).await;
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
        #[cfg(not(feature = "libcamera"))]
        {
            let _ = (backend, err);
            false
        }
        #[cfg(feature = "libcamera")]
        {
            if backend != BackendKind::Libcamera || !err.requires_disabling_tdn() {
                return false;
            }

            let mut updated = false;
            for (id, value) in &mut self.controls {
                if *id == LIBCAMERA_NOISE_REDUCTION_MODE {
                    if !matches!(value, ControlValue::Int(0)) {
                        *value = ControlValue::Int(0);
                        updated = true;
                    }
                    if self.tdn_output_mode != TdnOutputMode::Off {
                        updated = true;
                    }
                    self.tdn_output_mode = TdnOutputMode::Off;
                    return updated;
                }
            }

            self.controls
                .push((LIBCAMERA_NOISE_REDUCTION_MODE, ControlValue::Int(0)));
            self.tdn_output_mode = TdnOutputMode::Off;
            true
        }
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
/// ```rust,no_run
/// use styx::prelude::*;
///
/// let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
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

fn log_start_retry(
    backend: BackendKind,
    attempt: usize,
    max_attempts: usize,
    retry_action: &'static str,
    err: &CaptureError,
) {
    crate::trace::warn!(
        ?backend,
        attempt,
        max_attempts,
        retry_action,
        error_code = err.code(),
        error = %err,
        "capture start retry scheduled"
    );
}

fn pick_mode(backend: &ProbedBackend, mode: Option<ModeId>) -> Result<&Mode, CaptureError> {
    if backend.descriptor.modes.is_empty() {
        return Err(CaptureError::NoModes);
    }
    if let Some(id) = mode {
        let requested = &id.format;
        let is_bayer = requested.code.is_bayer_raw();

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
mod tests;

pub(super) fn default_interval(mode: &Mode) -> Option<Interval> {
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

#[cfg(feature = "async")]
async fn sleep_before_retry_async(delay: Duration) {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
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
