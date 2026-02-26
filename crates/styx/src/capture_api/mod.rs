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
pub use request::{CaptureError, CaptureRequest, TdnOutputMode, start_capture};
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
use std::collections::{HashMap, HashSet};
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

#[cfg(feature = "file-backend")]
fn sanitize_file_control_token(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_underscore = false;
    for ch in name.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '_'
        };
        if mapped == '_' {
            if prev_underscore {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        out.push(mapped);
    }
    let token = out.trim_matches('_');
    if token.is_empty() {
        "file".to_string()
    } else {
        token.to_string()
    }
}

#[cfg(feature = "file-backend")]
fn unique_file_control_token(base: &str, seen: &mut HashMap<String, usize>) -> String {
    let entry = seen.entry(base.to_string()).or_insert(0);
    *entry = entry.saturating_add(1);
    if *entry == 1 {
        base.to_string()
    } else {
        format!("{base}_{}", *entry)
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

/// Create a synthetic file device that replays image/video files as frames.
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
    let media_infos: Vec<_> = paths
        .iter()
        .map(crate::capture_api::file_backend::inspect_file_media)
        .collect();

    let mut seen_resolutions = HashSet::<(u32, u32)>::new();
    let mut resolutions = Vec::new();
    for info in &media_infos {
        if let Some(res) = info.resolution {
            let key = (res.width.get(), res.height.get());
            if seen_resolutions.insert(key) {
                resolutions.push(res);
            }
        }
    }
    if resolutions.is_empty() {
        resolutions.push(Resolution::new(1, 1).unwrap());
    }
    resolutions.sort_by(|a, b| {
        let area_a = u64::from(a.width.get()).saturating_mul(u64::from(a.height.get()));
        let area_b = u64::from(b.width.get()).saturating_mul(u64::from(b.height.get()));
        area_b
            .cmp(&area_a)
            .then_with(|| b.width.get().cmp(&a.width.get()))
            .then_with(|| b.height.get().cmp(&a.height.get()))
    });

    let interval = interval_from_fps(fps.max(1));
    let modes = resolutions
        .iter()
        .map(|res| {
            let format = MediaFormat::new(FourCc::new(*b"RG24"), *res, ColorSpace::Srgb);
            Mode {
                id: ModeId {
                    format,
                    interval: Some(interval),
                },
                format,
                intervals: smallvec::smallvec![interval],
                interval_stepwise: None,
            }
        })
        .collect::<Vec<_>>();

    let mut controls = Vec::new();
    let mut seen_tokens = HashMap::<String, usize>::new();
    let mut image_index = 0usize;
    #[cfg(feature = "file-backend-video")]
    let mut video_index = 0usize;
    for info in &media_infos {
        let base = sanitize_file_control_token(&info.name);
        let token = unique_file_control_token(&base, &mut seen_tokens);
        match info.kind {
            #[cfg(feature = "file-backend-video")]
            crate::capture_api::file_backend::FileMediaKind::Video => {
                let frame_max = info
                    .frame_count
                    .map(|count| count.saturating_sub(1))
                    .unwrap_or(u32::MAX);
                let stop_default = info
                    .frame_count
                    .map(|count| count.saturating_sub(1))
                    .unwrap_or(0);
                controls.push(ControlMeta {
                    id: crate::capture_api::file_backend::control_id_file_video_playback_speed(
                        video_index,
                    ),
                    name: crate::capture_api::file_backend::control_name_file_video_playback_speed(
                        &token,
                    ),
                    kind: ControlKind::Float,
                    access: Access::ReadWrite,
                    min: ControlValue::Float(0.05),
                    max: ControlValue::Float(16.0),
                    default: ControlValue::Float(1.0),
                    step: Some(ControlValue::Float(0.05)),
                    menu: None,
                    metadata: ControlMetadata::default(),
                });
                controls.push(ControlMeta {
                    id: crate::capture_api::file_backend::control_id_file_video_start_frame(
                        video_index,
                    ),
                    name: crate::capture_api::file_backend::control_name_file_video_start_frame(
                        &token,
                    ),
                    kind: ControlKind::Uint,
                    access: Access::ReadWrite,
                    min: ControlValue::Uint(0),
                    max: ControlValue::Uint(frame_max),
                    default: ControlValue::Uint(0),
                    step: Some(ControlValue::Uint(1)),
                    menu: None,
                    metadata: ControlMetadata::default(),
                });
                controls.push(ControlMeta {
                    id: crate::capture_api::file_backend::control_id_file_video_stop_frame(
                        video_index,
                    ),
                    name: crate::capture_api::file_backend::control_name_file_video_stop_frame(
                        &token,
                    ),
                    kind: ControlKind::Uint,
                    access: Access::ReadWrite,
                    min: ControlValue::Uint(0),
                    max: ControlValue::Uint(frame_max),
                    default: ControlValue::Uint(stop_default),
                    step: Some(ControlValue::Uint(1)),
                    menu: None,
                    metadata: ControlMetadata::default(),
                });
                video_index = video_index.saturating_add(1);
            }
            crate::capture_api::file_backend::FileMediaKind::Image => {
                controls.push(ControlMeta {
                    id: crate::capture_api::file_backend::control_id_file_image_duration_frames(
                        image_index,
                    ),
                    name: crate::capture_api::file_backend::control_name_file_image_duration_frames(
                        &token,
                    ),
                    kind: ControlKind::Uint,
                    access: Access::ReadWrite,
                    min: ControlValue::Uint(1),
                    max: ControlValue::Uint(3600),
                    default: ControlValue::Uint(1),
                    step: Some(ControlValue::Uint(1)),
                    menu: None,
                    metadata: ControlMetadata::default(),
                });
                image_index = image_index.saturating_add(1);
            }
            crate::capture_api::file_backend::FileMediaKind::Unknown => {}
        }
    }

    let descriptor = CaptureDescriptor {
        modes: modes.clone(),
        controls,
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
