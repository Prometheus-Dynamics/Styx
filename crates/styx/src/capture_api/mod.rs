//! Capture helpers, request builders, and backend constructors.
//!
//! Most users will interact with `CaptureRequest` or `MediaPipelineBuilder`.
//!
//! # Example
//! ```rust,ignore
//! use styx::prelude::*;
//!
//! let device = probe_all().into_iter().next().expect("device");
//! let handle = CaptureRequest::new(&device).start()?;
//! let _ = handle.recv();
//! # Ok::<(), styx::capture_api::CaptureError>(())
//! ```
pub mod controls;
#[cfg(any(feature = "netcam", feature = "file-backend"))]
pub(super) mod ffmpeg_util;
#[cfg(feature = "file-backend")]
pub(super) mod file_backend;
#[cfg(feature = "libcamera")]
pub(super) mod libcamera_backend;
#[cfg(feature = "netcam")]
pub(super) mod netcam_backend;
#[cfg(feature = "v4l2")]
pub(super) mod v4l2_backend;
pub(super) mod virtual_backend;

pub use handle::{CaptureHandle, ControlPlane, WorkerHandle};
pub use request::{CaptureError, CaptureRequest, start_capture};
pub use tunables::{
    CaptureTunables, DEFAULT_NETCAM_BACKOFF_MAX_MS, DEFAULT_NETCAM_BACKOFF_START_MS,
    DEFAULT_NETCAM_TIMEOUT_SECS, DEFAULT_POOL_BYTES, DEFAULT_POOL_MIN, DEFAULT_POOL_SPARE,
    DEFAULT_QUEUE_DEPTH, NetcamTunables, StyxConfig, set_capture_tunables, set_netcam_tunables,
};
#[allow(unused_imports)]
pub(crate) use tunables::{capture_pool_limits, capture_queue_depth, netcam_tunables};

#[allow(unused_imports)]
#[cfg(any(feature = "netcam", feature = "file-backend"))]
use crate::{BackendHandle, DeviceIdentity};
#[allow(unused_imports)]
use crate::{BackendKind, ProbedBackend, ProbedDevice};
#[cfg(feature = "file-backend")]
use image::GenericImageView;
#[cfg(any(feature = "netcam", feature = "file-backend"))]
use std::num::NonZeroU32;
use styx_capture::prelude::*;

mod handle;
mod request;
mod tunables;

#[cfg(any(feature = "netcam", feature = "file-backend"))]
fn interval_from_fps(fps: u32) -> Interval {
    Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(fps.max(1)).unwrap(),
    }
}

/// Create a synthetic netcam device (MJPEG over HTTP) for manual wiring.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let device = make_netcam_device("cam", "http://cam/mjpeg", 640, 480, 30);
/// let handle = CaptureRequest::new(&device).start()?;
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
#[cfg(feature = "netcam")]
pub fn make_netcam_device(
    name: &str,
    url: &str,
    width: u32,
    height: u32,
    fps: u32,
) -> ProbedDevice {
    let res = Resolution::new(width, height).unwrap_or_else(|| Resolution::new(1, 1).unwrap());
    let interval = interval_from_fps(fps.max(1));
    let format = MediaFormat::new(FourCc::new(*b"MJPG"), res, ColorSpace::Srgb);
    let mode = Mode {
        id: ModeId {
            format: format.clone(),
            interval: Some(interval),
        },
        format,
        intervals: smallvec::smallvec![interval],
        interval_stepwise: None,
    };
    let descriptor = CaptureDescriptor {
        modes: vec![mode.clone()],
        controls: Vec::new(),
    };
    let backend = ProbedBackend {
        kind: BackendKind::Netcam,
        handle: BackendHandle::Netcam {
            url: url.to_string(),
            width,
            height,
            fps,
        },
        descriptor,
        properties: vec![("url".into(), url.to_string())],
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: name.to_string(),
            keys: vec![url.to_string()],
        },
        backends: vec![backend],
    }
}

/// Create a synthetic file device that replays images as frames.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let device = make_file_device("replay", vec!["frame.png".into()], 30, true);
/// let handle = CaptureRequest::new(&device).start()?;
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
#[cfg(feature = "file-backend")]
pub fn make_file_device(
    name: &str,
    paths: Vec<std::path::PathBuf>,
    fps: u32,
    loop_forever: bool,
) -> ProbedDevice {
    let mut res = Resolution::new(1, 1).unwrap();
    for p in &paths {
        if let Ok(bytes) = std::fs::read(p) {
            if let Ok(img) = image::load_from_memory(&bytes) {
                let (w, h) = img.dimensions();
                if let Some(r) = Resolution::new(w, h) {
                    res = r;
                }
                break;
            }
        }
    }
    let interval = interval_from_fps(fps.max(1));
    let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
    let mode = Mode {
        id: ModeId {
            format: format.clone(),
            interval: Some(interval),
        },
        format,
        intervals: smallvec::smallvec![interval],
        interval_stepwise: None,
    };
    let descriptor = CaptureDescriptor {
        modes: vec![mode.clone()],
        controls: vec![
            ControlMeta {
                id: crate::capture_api::file_backend::CTRL_FILE_DURATION_MS,
                name: "file.duration_ms".into(),
                kind: ControlKind::Uint,
                access: Access::ReadWrite,
                min: ControlValue::Uint(1),
                max: ControlValue::Uint(120_000),
                default: ControlValue::Uint((1_000 / fps.max(1)) as u32),
                step: Some(ControlValue::Uint(1)),
                menu: None,
            },
            ControlMeta {
                id: crate::capture_api::file_backend::CTRL_FILE_START_MS,
                name: "file.start_ms".into(),
                kind: ControlKind::Uint,
                access: Access::ReadWrite,
                min: ControlValue::Uint(0),
                max: ControlValue::Uint(u32::MAX),
                default: ControlValue::Uint(0),
                step: Some(ControlValue::Uint(1)),
                menu: None,
            },
            ControlMeta {
                id: crate::capture_api::file_backend::CTRL_FILE_STOP_MS,
                name: "file.stop_ms".into(),
                kind: ControlKind::Uint,
                access: Access::ReadWrite,
                min: ControlValue::Uint(0),
                max: ControlValue::Uint(u32::MAX),
                default: ControlValue::Uint(0),
                step: Some(ControlValue::Uint(1)),
                menu: None,
            },
            ControlMeta {
                id: crate::capture_api::file_backend::CTRL_FILE_IMAGE_FPS,
                name: "file.image_fps".into(),
                kind: ControlKind::Uint,
                access: Access::ReadWrite,
                min: ControlValue::Uint(1),
                max: ControlValue::Uint(240),
                default: ControlValue::Uint(60),
                step: Some(ControlValue::Uint(1)),
                menu: None,
            },
            ControlMeta {
                id: crate::capture_api::file_backend::CTRL_FILE_VIDEO_FPS,
                name: "file.video_fps".into(),
                kind: ControlKind::Uint,
                access: Access::ReadWrite,
                min: ControlValue::Uint(0),
                max: ControlValue::Uint(240),
                default: ControlValue::Uint(0),
                step: Some(ControlValue::Uint(1)),
                menu: None,
            },
        ],
    };
    let backend = ProbedBackend {
        kind: BackendKind::File,
        handle: BackendHandle::File {
            paths: paths.clone(),
            fps,
            loop_forever,
        },
        descriptor,
        properties: vec![("paths".into(), format!("{}", paths.len()))],
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: name.to_string(),
            keys: paths
                .iter()
                .filter_map(|p| p.to_str().map(|s| s.to_string()))
                .collect(),
        },
        backends: vec![backend],
    }
}
