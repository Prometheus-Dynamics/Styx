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
    /// A virtual camera whose descriptor lists controls: values are checked and kept.
    VirtualControls(std::sync::Arc<super::virtual_backend::VirtualControls>),
    /// A native camera: frame-accurate typed controls.
    #[cfg(feature = "native")]
    Native {
        controls: styx_native::CameraControls,
        /// A processed mode's 3A loop: its controls and state (AE state, colour temperature).
        processed: Option<std::sync::Arc<super::native_isp::LoopControls>>,
    },
    /// A UVC camera driven from userspace: V4L2 control ids, as `uvcvideo`.
    #[cfg(feature = "uvc")]
    Uvc {
        device: styx_uvc::UvcDevice,
    },
    /// A reconnecting capture: controls go to whichever backend capture is running and are
    /// re-applied after a reconnect.
    Supervised(std::sync::Arc<super::supervisor::SupervisedCapture>),
}

impl ControlPlane {
    /// A control's current value, as [`CaptureHandle::get_control`](super::CaptureHandle::get_control)
    /// reads it, from any thread: a consumer can read controls off its frame thread (a
    /// libcamera read waits for the capture thread, up to a frame period per control).
    pub fn get_control(&self, id: ControlId) -> Result<ControlValue, CaptureError> {
        read_control_from_plane(self, id)
    }

    /// Set a control, as [`CaptureHandle::set_control`](super::CaptureHandle::set_control)
    /// does, from any thread.
    pub fn set_control(&self, id: ControlId, value: ControlValue) -> Result<(), CaptureError> {
        apply_control_to_plane(self, id, value)
    }
}

impl super::CaptureHandle {
    /// The capture's controls, to read or set from another thread ([`ControlPlane::get_control`]).
    pub fn control_plane(&self) -> ControlPlane {
        self.control.clone()
    }
}

pub(crate) fn apply_control_to_plane(
    control: &ControlPlane,
    id: ControlId,
    _value: ControlValue,
) -> Result<(), CaptureError> {
    let backend = control_plane_backend(control);
    let started = Instant::now();
    crate::trace::debug!(
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
        ControlPlane::VirtualControls(state) => state.apply(id, _value),
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
            controls,
            processed,
        } => match processed.as_ref().and_then(|p| p.apply(id, &_value)) {
            Some(result) => result,
            None => super::native_backend::apply_control(controls, id, &_value),
        },
        #[cfg(feature = "uvc")]
        ControlPlane::Uvc { device } => super::uvc_backend::apply_control(device, id, &_value),
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

/// [`apply_control_to_plane`], and for frame-exact backends (a native camera's raw modes) the
/// first frame (sensor sequence, as `NativeFrameMeta::sequence`) predicted to use the value.
pub(crate) fn apply_control_landing(
    control: &ControlPlane,
    id: ControlId,
    value: ControlValue,
) -> Result<Option<u64>, CaptureError> {
    match control {
        ControlPlane::Supervised(shared) => match shared.current_control() {
            Ok(inner) => {
                let result = apply_control_landing(&inner, id, value.clone());
                if result.is_ok() {
                    shared.remember_control(id, value);
                }
                result
            }
            Err(_) => apply_control_to_plane(control, id, value).map(|()| None),
        },
        #[cfg(feature = "native")]
        ControlPlane::Native {
            controls,
            processed,
        } => {
            let started = Instant::now();
            let result = match processed.as_ref().and_then(|p| p.apply(id, &value)) {
                Some(result) => result.map(|()| None),
                None => super::native_backend::apply_control_landing(controls, id, &value),
            };
            log_control_result("native", id, "set", started, &result);
            result
        }
        _ => apply_control_to_plane(control, id, value).map(|()| None),
    }
}

impl super::CaptureHandle {
    /// [`CaptureHandle::set_control`](super::CaptureHandle::set_control), and on frame-exact
    /// backends (a native camera's raw modes) the first frame predicted to use the value (its
    /// sensor sequence, as `NativeFrameMeta::sequence`); `None` elsewhere.
    pub fn set_control_landing(
        &self,
        id: ControlId,
        value: ControlValue,
    ) -> Result<Option<u64>, CaptureError> {
        let result = apply_control_landing(&self.control, id, value);
        self.record_control_result(&result);
        result
    }

    /// The controls this capture lists (its backend's descriptor).
    pub fn control_metas(&self) -> &[styx_capture::prelude::ControlMeta] {
        &self.descriptor.controls
    }
}

pub(crate) fn read_control_from_plane(
    control: &ControlPlane,
    id: ControlId,
) -> Result<ControlValue, CaptureError> {
    let backend = control_plane_backend(control);
    let started = Instant::now();
    crate::trace::debug!(
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
            processed,
        } => match processed.as_ref().and_then(|p| p.read(id)) {
            Some(value) => Ok(value),
            None => super::native_backend::read_control(controls, id),
        },
        #[cfg(feature = "uvc")]
        ControlPlane::Uvc { device } => super::uvc_backend::read_control(device, id),
        #[cfg(feature = "file-backend")]
        ControlPlane::File { state } => file_backend::read_file_control(state, id),
        #[cfg(feature = "simulation-bevy")]
        ControlPlane::Simulation { state } => {
            simulation_backend::read_simulation_control(state, id)
        }
        ControlPlane::VirtualControls(state) => state.read(id),
        _ => Err(CaptureError::ControlUnsupported),
    };
    log_control_result(backend, id, "get", started, &result);
    result
}

fn control_plane_backend(control: &ControlPlane) -> &'static str {
    match control {
        ControlPlane::None => "none",
        ControlPlane::Supervised(_) => "supervised",
        ControlPlane::Virtual | ControlPlane::VirtualControls(_) => "virtual",
        #[cfg(feature = "native")]
        ControlPlane::Native { .. } => "native",
        #[cfg(feature = "v4l2")]
        ControlPlane::V4l2 { .. } => "v4l2",
        #[cfg(feature = "uvc")]
        ControlPlane::Uvc { .. } => "uvc",
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
        Ok(_) => crate::trace::debug!(
            backend,
            control_id = id.0,
            operation,
            elapsed_ms,
            "control request completed"
        ),
        Err(err) => crate::trace::warn!(
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
