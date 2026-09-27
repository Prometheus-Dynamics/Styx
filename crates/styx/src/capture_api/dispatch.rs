//! Starting the backend capture for a resolved request.

#[cfg(feature = "file-backend")]
use super::file_backend;
use super::handle::{CaptureHandle, CaptureQueue};
#[cfg(feature = "libcamera")]
use super::libcamera_backend;
#[cfg(feature = "netcam")]
use super::netcam_backend;
use super::request::{CaptureError, TdnOutputMode};
use super::tunables::StyxConfig;
#[cfg(feature = "v4l2")]
use super::v4l2_backend;
use super::virtual_backend;
#[cfg(feature = "simulation-bevy")]
use crate::simulation::backend as simulation_backend;
use crate::{BackendKind, ProbedBackend};
use styx_capture::prelude::*;

/// `queue` replaces the queue the backend would create (libcamera, V4L2 and virtual); see
/// [`super::supervisor`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_backend(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    descriptor: CaptureDescriptor,
    _controls: Vec<(ControlId, ControlValue)>,
    _tdn_output_mode: TdnOutputMode,
    config: &StyxConfig,
    _queue: Option<CaptureQueue>,
) -> Result<CaptureHandle, CaptureError> {
    match backend.kind {
        BackendKind::Virtual => {
            virtual_backend::start_virtual(mode, interval, descriptor, config, _queue)
        }
        #[cfg(feature = "v4l2")]
        BackendKind::V4l2 => v4l2_backend::start_v4l2(
            backend, mode, interval, _controls, descriptor, config, _queue,
        ),
        #[cfg(not(feature = "v4l2"))]
        BackendKind::V4l2 => Err(CaptureError::BackendMissing(BackendKind::V4l2)),
        #[cfg(feature = "libcamera")]
        BackendKind::Libcamera => libcamera_backend::start_libcamera(
            backend,
            mode,
            interval,
            _controls,
            descriptor,
            _tdn_output_mode,
            config,
            _queue,
        ),
        #[cfg(not(feature = "libcamera"))]
        BackendKind::Libcamera => Err(CaptureError::BackendMissing(BackendKind::Libcamera)),
        #[cfg(feature = "netcam")]
        BackendKind::Netcam => {
            netcam_backend::start_netcam(backend, mode, interval, descriptor, config)
        }
        #[cfg(not(feature = "netcam"))]
        BackendKind::Netcam => Err(CaptureError::BackendMissing(BackendKind::Netcam)),
        #[cfg(feature = "file-backend")]
        BackendKind::File => {
            file_backend::start_file(backend, mode, interval, _controls, descriptor, config)
        }
        #[cfg(not(feature = "file-backend"))]
        BackendKind::File => Err(CaptureError::BackendMissing(BackendKind::File)),
        #[cfg(feature = "simulation-bevy")]
        BackendKind::Simulation => simulation_backend::start_simulation(
            backend, mode, interval, _controls, descriptor, config,
        ),
        #[cfg(not(feature = "simulation-bevy"))]
        BackendKind::Simulation => Err(CaptureError::BackendMissing(BackendKind::Simulation)),
    }
}
