use std::time::Instant;

#[cfg(feature = "libcamera")]
use crate::capture_api::libcamera_backend::{ControlMessage, PendingControlState};
#[cfg(feature = "libcamera")]
use parking_lot::Mutex;

use super::CaptureError;
#[cfg(feature = "v4l2")]
use super::controls::{apply_v4l2_controls, read_v4l2_control};
#[cfg(feature = "file-backend")]
use super::file_backend;
#[cfg(feature = "simulation-bevy")]
use crate::simulation::backend as simulation_backend;
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
        pending: std::sync::Arc<Mutex<PendingControlState>>,
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
    /// A native camera: frame-accurate typed controls.
    #[cfg(feature = "native")]
    Native {
        controls: styx_native::CameraControls,
        /// The 3A loop's AE state of a processed mode (see `native_backend::controls::AE_STATE`).
        ae_state: Option<std::sync::Arc<std::sync::atomic::AtomicI32>>,
        /// A processed mode's flicker avoidance: the mode asked for (`AE_FLICKER_MODE`) and the
        /// period detected in microseconds (`AE_FLICKER_DETECTED`).
        flicker: Option<std::sync::Arc<[std::sync::atomic::AtomicI32; 2]>>,
    },
    /// A reconnecting capture: controls go to whichever backend capture is running and are
    /// re-applied after a reconnect.
    Supervised(std::sync::Arc<super::supervisor::SupervisedCapture>),
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
        // Stopped while idle: the control is applied when streaming starts again.
        ControlPlane::Supervised(shared) => match shared.current_control() {
            Ok(inner) => {
                // Also while paused: the paused capture applies it when streaming again.
                let result = apply_control_to_plane(&inner, id, _value.clone());
                if result.is_ok() {
                    shared.remember_control(id, _value);
                }
                result
            }
            // Stopped while idle: applied when it starts again.
            Err(_) if shared.is_idle() => {
                shared.remember_control(id, _value);
                Ok(())
            }
            Err(err) => Err(err),
        },
        ControlPlane::None | ControlPlane::Virtual => Err(CaptureError::ControlUnsupported),
        #[cfg(feature = "v4l2")]
        ControlPlane::V4l2 { path } => apply_v4l2_controls(path, &[(id, _value)]),
        #[cfg(feature = "libcamera")]
        ControlPlane::Libcamera { tx, pending, .. } => {
            {
                let mut guard = pending.lock();
                if matches!(_value, ControlValue::None) {
                    guard.insert(id, None);
                } else {
                    guard.insert(id, Some(_value));
                }
            }
            tx.send(ControlMessage::Wake)
                .map_err(|_| CaptureError::control_apply("libcamera channel closed"))
        }
        #[cfg(feature = "native")]
        ControlPlane::Native {
            controls, flicker, ..
        } => {
            if id == super::native_backend::controls::AE_FLICKER_MODE {
                let state = flicker.as_ref().ok_or(CaptureError::ControlUnsupported)?;
                let mode = match _value {
                    ControlValue::Int(v) => Some(i64::from(v)),
                    ControlValue::Uint(v) => Some(i64::from(v)),
                    _ => None,
                }
                .and_then(super::NativeFlicker::from_control_value)
                .ok_or_else(|| {
                    CaptureError::control_apply("AE flicker mode: 0 off, 1 50 Hz, 2 60 Hz, 3 auto")
                })?;
                state[0].store(mode.control_value(), std::sync::atomic::Ordering::Release);
                return Ok(());
            }
            super::native_backend::apply_control(controls, id, &_value)
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
        ControlPlane::Supervised(shared) => match shared.current_control() {
            Ok(inner) => read_control_from_plane(&inner, id),
            Err(_) if shared.is_idle() => shared
                .remembered_control(id)
                .ok_or_else(|| CaptureError::Disconnected("camera is stopped while idle".into())),
            Err(err) => Err(err),
        },
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
        #[cfg(feature = "native")]
        ControlPlane::Native {
            controls,
            ae_state,
            flicker,
        } => {
            use super::native_backend::controls::{AE_FLICKER_DETECTED, AE_FLICKER_MODE};
            if id == AE_FLICKER_MODE || id == AE_FLICKER_DETECTED {
                let i = usize::from(id == AE_FLICKER_DETECTED);
                return flicker
                    .as_ref()
                    .map(|s| ControlValue::Int(s[i].load(std::sync::atomic::Ordering::Acquire)))
                    .ok_or(CaptureError::ControlUnsupported);
            }
            if id == super::native_backend::controls::AE_STATE {
                return ae_state
                    .as_ref()
                    .map(|s| ControlValue::Int(s.load(std::sync::atomic::Ordering::Acquire)))
                    .ok_or(CaptureError::ControlUnsupported);
            }
            super::native_backend::read_control(controls, id)
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
        ControlPlane::Supervised(_) => "supervised",
        ControlPlane::Virtual => "virtual",
        #[cfg(feature = "native")]
        ControlPlane::Native { .. } => "native",
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
