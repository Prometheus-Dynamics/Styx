//! Capture start and runtime errors.

use crate::BackendKind;
use styx_capture::prelude::*;

/// Errors starting a capture session.
///
/// # Example
/// ```rust
/// use styx::prelude::*;
///
/// let err = CaptureError::BackendMissing(BackendKind::V4l2);
/// assert_eq!(err.code(), "backend_missing");
/// ```
#[derive(Debug, Clone, thiserror::Error)]
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
    /// The camera went away while capturing (unplugged, driver reset).
    #[error("camera disconnected: {0}")]
    Disconnected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlApplyKind {
    Other,
    SetControlsRejected,
    InvalidArgument,
    PermissionDenied,
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
            CaptureError::Disconnected(_) => "disconnected",
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
                | CaptureError::Disconnected(_)
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
