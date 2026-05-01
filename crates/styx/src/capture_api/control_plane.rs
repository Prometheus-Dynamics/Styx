use std::time::Instant;

#[cfg(feature = "libcamera")]
use crate::capture_api::libcamera_backend::{ControlMessage, PendingControlState};

use super::CaptureError;
#[cfg(feature = "v4l2")]
use super::controls::{apply_v4l2_controls, read_v4l2_control};
#[cfg(feature = "file-backend")]
use super::file_backend;
#[cfg(feature = "simulation-bevy")]
use super::simulation_backend;
use styx_capture::prelude::{ControlId, ControlValue};

/// Control plane handle for applying backend-specific controls.
#[derive(Debug, Clone)]
pub enum ControlPlane {
    None,
    #[cfg(feature = "v4l2")]
    V4l2 {
        path: String,
    },
    #[cfg(feature = "libcamera")]
    Libcamera {
        tx: std::sync::mpsc::Sender<ControlMessage>,
        pending: std::sync::Arc<std::sync::Mutex<PendingControlState>>,
        response_timeout: std::time::Duration,
    },
    #[cfg(feature = "file-backend")]
    File {
        state: file_backend::FileControlStateHandle,
    },
    #[cfg(feature = "simulation-bevy")]
    Simulation {
        state: simulation_backend::SimulationControlStateHandle,
    },
    Virtual,
}

pub(crate) fn apply_control_to_plane(
    control: &ControlPlane,
    id: ControlId,
    _value: ControlValue,
) -> Result<(), CaptureError> {
    let backend = control_plane_backend(control);
    let started = Instant::now();
    tracing::debug!(
        backend,
        control_id = id.0,
        operation = "set",
        "control request started"
    );
    let result = match control {
        ControlPlane::None | ControlPlane::Virtual => Err(CaptureError::ControlUnsupported),
        #[cfg(feature = "v4l2")]
        ControlPlane::V4l2 { path } => apply_v4l2_controls(path, &[(id, _value)]),
        #[cfg(feature = "libcamera")]
        ControlPlane::Libcamera { tx, pending, .. } => {
            {
                let mut guard = pending
                    .lock()
                    .map_err(|_| CaptureError::control_apply("libcamera pending lock poisoned"))?;
                if matches!(_value, ControlValue::None) {
                    guard.insert(id, None);
                } else {
                    guard.insert(id, Some(_value));
                }
            }
            tx.send(ControlMessage::Wake)
                .map_err(|_| CaptureError::control_apply("libcamera channel closed"))
        }
        #[cfg(feature = "file-backend")]
        ControlPlane::File { state } => file_backend::apply_file_control(state, id, _value),
        #[cfg(feature = "simulation-bevy")]
        ControlPlane::Simulation { state } => {
            simulation_backend::apply_simulation_control(state, id, _value)
        }
    };
    log_control_result(backend, id, "set", started, &result);
    result
}

pub(crate) fn read_control_from_plane(
    control: &ControlPlane,
    id: ControlId,
) -> Result<ControlValue, CaptureError> {
    let backend = control_plane_backend(control);
    let started = Instant::now();
    tracing::debug!(
        backend,
        control_id = id.0,
        operation = "get",
        "control request started"
    );
    let result = match control {
        #[cfg(feature = "v4l2")]
        ControlPlane::V4l2 { path } => read_v4l2_control(path, id),
        #[cfg(feature = "libcamera")]
        ControlPlane::Libcamera {
            tx,
            response_timeout,
            ..
        } => {
            let (resp_tx, resp_rx) = std::sync::mpsc::channel();
            tx.send(ControlMessage::Get(id, resp_tx))
                .map_err(|_| CaptureError::control_apply("libcamera channel closed"))?;
            resp_rx
                .recv_timeout(*response_timeout)
                .map_err(|err| match err {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        CaptureError::control_apply(format!(
                            "libcamera control response timed out after {} ms",
                            response_timeout.as_millis()
                        ))
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        CaptureError::control_apply("libcamera response closed")
                    }
                })?
        }
        #[cfg(feature = "file-backend")]
        ControlPlane::File { state } => file_backend::read_file_control(state, id),
        #[cfg(feature = "simulation-bevy")]
        ControlPlane::Simulation { state } => {
            simulation_backend::read_simulation_control(state, id)
        }
        _ => Err(CaptureError::ControlUnsupported),
    };
    log_control_result(backend, id, "get", started, &result);
    result
}

fn control_plane_backend(control: &ControlPlane) -> &'static str {
    match control {
        ControlPlane::None => "none",
        ControlPlane::Virtual => "virtual",
        #[cfg(feature = "v4l2")]
        ControlPlane::V4l2 { .. } => "v4l2",
        #[cfg(feature = "libcamera")]
        ControlPlane::Libcamera { .. } => "libcamera",
        #[cfg(feature = "file-backend")]
        ControlPlane::File { .. } => "file",
        #[cfg(feature = "simulation-bevy")]
        ControlPlane::Simulation { .. } => "simulation",
    }
}

fn log_control_result<T>(
    backend: &'static str,
    id: ControlId,
    operation: &'static str,
    started: Instant,
    result: &Result<T, CaptureError>,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(_) => tracing::debug!(
            backend,
            control_id = id.0,
            operation,
            elapsed_ms,
            "control request completed"
        ),
        Err(err) => tracing::warn!(
            backend,
            control_id = id.0,
            operation,
            elapsed_ms,
            error_code = err.code(),
            error = %err,
            "control request failed"
        ),
    }
}
