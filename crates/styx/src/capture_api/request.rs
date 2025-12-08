use crate::{BackendKind, ProbedBackend, ProbedDevice};
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
    #[error("mode {0:?} not advertised by backend")]
    InvalidMode(ModeId),
    #[error("capture config rejected: {0}")]
    InvalidConfig(String),
    #[error("backend {0:?} capture not implemented yet")]
    NotImplemented(BackendKind),
    #[error("control plane not available for backend")]
    ControlUnsupported,
    #[error("control apply failed: {0}")]
    ControlApply(String),
    #[error("backend error: {0}")]
    Backend(String),
}

impl CaptureError {
    /// Stable string code for error classification.
    pub fn code(&self) -> &'static str {
        match self {
            CaptureError::NoBackend => "no_backend",
            CaptureError::BackendUnavailable(_) => "backend_unavailable",
            CaptureError::BackendMissing(_) => "backend_missing",
            CaptureError::NoModes => "no_modes",
            CaptureError::InvalidMode(_) => "invalid_mode",
            CaptureError::InvalidConfig(_) => "invalid_config",
            CaptureError::NotImplemented(_) => "not_implemented",
            CaptureError::ControlUnsupported => "control_unsupported",
            CaptureError::ControlApply(_) => "control_apply_failed",
            CaptureError::Backend(_) => "backend_error",
        }
    }

    /// Whether the error may succeed when retried.
    pub fn retryable(&self) -> bool {
        matches!(self, CaptureError::BackendUnavailable(_) | CaptureError::Backend(_))
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
pub struct CaptureRequest<'a> {
    device: &'a ProbedDevice,
    backend: Option<BackendKind>,
    mode: Option<ModeId>,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    enable_tdn_output: bool,
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
            enable_tdn_output: false,
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
        self.enable_tdn_output = enable;
        self
    }

    /// Start capture after validating backend/mode/interval/controls.
    ///
    /// Returns a running `CaptureHandle` that can receive frames.
    pub fn start(self) -> Result<super::handle::CaptureHandle, CaptureError> {
        let backend = pick_backend(self.device, self.backend)?;
        let mode = pick_mode(backend, self.mode)?;
        validate_config(backend, mode, self.interval, &self.controls)?;
        let interval = self.interval.or_else(|| default_interval(mode));
        start_backend(backend, mode.clone(), interval, self.controls, self.enable_tdn_output)
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
}

fn default_interval(mode: &Mode) -> Option<Interval> {
    mode.intervals
        .first()
        .copied()
        .or_else(|| mode.interval_stepwise.map(|s| s.min))
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
