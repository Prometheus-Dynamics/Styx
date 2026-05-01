//! Capture helpers, request builders, and backend constructors.
//!
//! Most users will interact with `CaptureRequest` or `MediaPipelineBuilder`.
//!
//! # Example
//! ```rust,no_run
//! use styx::prelude::*;
//!
//! let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
//! let handle = CaptureRequest::new(&device).start()?;
//! let _ = handle.recv();
//! # Ok::<(), styx::capture_api::CaptureError>(())
//! ```
mod control_plane;
pub mod controls;
#[cfg(any(
    feature = "netcam",
    feature = "file-backend",
    feature = "simulation-bevy"
))]
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

pub use control_plane::ControlPlane;
#[cfg(feature = "graph-pipeline")]
pub(crate) use control_plane::{apply_control_to_plane, read_control_from_plane};
pub use handle::{CaptureFrameIter, CaptureHandle, WorkerHandle};
pub use request::{
    CameraFormat, CameraIntervalPreference, CameraRequest, CameraStartPolicy, CaptureError,
    CaptureRequest, CaptureSource, CaptureStartPolicy, ControlApplyKind, SelectedCamera,
    TdnOutputMode, start_capture,
};
pub use tunables::{
    BackendConfig, CaptureConfig, CaptureTunables, CodecConfig, DEFAULT_CAPTURE_IDLE_POLL_MS,
    DEFAULT_CAPTURE_QUEUE_SEND_TIMEOUT_MS, DEFAULT_LIBCAMERA_CONTROL_RESPONSE_TIMEOUT_MS,
    DEFAULT_LIBCAMERA_IDLE_DRAIN_POLL_MS, DEFAULT_LIBCAMERA_IDLE_DRAIN_TIMEOUT_MS,
    DEFAULT_LIBCAMERA_LOOKUP_POLL_MS, DEFAULT_LIBCAMERA_LOOKUP_TIMEOUT_MS,
    DEFAULT_LIBCAMERA_PREFAULT_REQUEST_POOLS, DEFAULT_LIBCAMERA_PROBE_CACHE_MS,
    DEFAULT_LIBCAMERA_REQUEST_POLL_MS, DEFAULT_LIBCAMERA_REQUEUE_STALL_TIMEOUT_MS,
    DEFAULT_LIBCAMERA_STOP_WHEN_IDLE, DEFAULT_NETCAM_BACKOFF_MAX_MS,
    DEFAULT_NETCAM_BACKOFF_START_MS, DEFAULT_NETCAM_MAX_JPEG_BYTES, DEFAULT_NETCAM_SEND_TIMEOUT_MS,
    DEFAULT_NETCAM_STOP_POLL_MS, DEFAULT_NETCAM_TIMEOUT_SECS, DEFAULT_POOL_BYTES, DEFAULT_POOL_MIN,
    DEFAULT_POOL_SPARE, DEFAULT_QUEUE_DEPTH, DEFAULT_V4L2_ERROR_BACKOFF_MS,
    DEFAULT_V4L2_MMAP_POLL_MS, DEFAULT_V4L2_SEND_TIMEOUT_MS, FileBackendConfig, LibcameraConfig,
    LibcameraProcessedStreamRole, NetcamConfig, NetcamTunables, StyxConfig, TransformConfig,
    V4l2Config,
};

// Release policy: these backend handle types are consumed only by feature-gated constructors, so
// some release feature combinations intentionally compile only a subset of the import list.
#[allow(unused_imports)]
use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};
#[cfg(feature = "file-backend")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "file-backend")]
use std::path::PathBuf;
use styx_capture::prelude::*;

#[derive(Clone, Debug)]
pub struct VirtualSourceConfig {
    pub name: String,
    pub format: FourCc,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub color_space: ColorSpace,
}

impl Default for VirtualSourceConfig {
    fn default() -> Self {
        Self {
            name: "virtual".into(),
            format: FourCc::RG24,
            width: 640,
            height: 360,
            fps: 30,
            color_space: ColorSpace::Srgb,
        }
    }
}

impl VirtualSourceConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn format(mut self, format: FourCc) -> Self {
        self.format = format;
        self
    }

    pub fn resolution(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }

    pub fn color_space(mut self, color_space: ColorSpace) -> Self {
        self.color_space = color_space;
        self
    }

    pub fn into_device(self) -> ProbedDevice {
        let format = MediaFormat::new(
            self.format,
            Resolution::new(self.width.max(1), self.height.max(1))
                .expect("virtual source dimensions are clamped to non-zero"),
            self.color_space,
        );
        make_virtual_device(
            &self.name,
            [Mode::with_interval(format, interval_from_fps(self.fps))],
        )
    }
}

pub type VirtualCaptureConfig = VirtualSourceConfig;

#[cfg(feature = "netcam")]
#[derive(Clone, Debug)]
pub struct NetcamSourceConfig {
    pub name: String,
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[cfg(feature = "netcam")]
impl NetcamSourceConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            name: "netcam".into(),
            url: url.into(),
            width: 640,
            height: 480,
            fps: 30,
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn resolution(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }

    pub fn into_device(self) -> ProbedDevice {
        make_netcam_device(&self.name, &self.url, self.width, self.height, self.fps)
    }
}

#[cfg(feature = "file-backend")]
#[derive(Clone, Debug)]
pub struct FileSourceConfig {
    pub name: String,
    pub paths: Vec<PathBuf>,
    pub fps: u32,
    pub loop_forever: bool,
}

#[cfg(feature = "file-backend")]
impl FileSourceConfig {
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            name: "file".into(),
            paths: paths.into_iter().collect(),
            fps: 30,
            loop_forever: false,
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }

    pub fn loop_forever(mut self, loop_forever: bool) -> Self {
        self.loop_forever = loop_forever;
        self
    }

    pub fn into_device(self) -> ProbedDevice {
        make_file_device(&self.name, self.paths, self.fps, self.loop_forever)
    }
}

pub(crate) mod handle;
#[cfg(test)]
mod handle_tests;
mod request;
mod tunables;

#[cfg(feature = "libcamera")]
pub(crate) use styx_libcamera::{LIBCAMERA_FRAME_DURATION_LIMITS, LIBCAMERA_NOISE_REDUCTION_MODE};

fn interval_from_fps(fps: u32) -> Interval {
    Interval::from_fps(fps.max(1)).expect("fps is clamped to non-zero")
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

/// Create a synthetic virtual device for manual wiring.
///
/// This is useful for examples, tests, demos, and fallback pipelines that should
/// exercise the same capture facade as real backends.
///
/// # Example
/// ```rust,no_run
/// use styx::prelude::*;
///
/// let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
/// let handle = CaptureRequest::new(&device).start()?;
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
pub fn make_virtual_device(name: &str, modes: impl IntoIterator<Item = Mode>) -> ProbedDevice {
    let mut modes = modes.into_iter().collect::<Vec<_>>();
    if modes.is_empty() {
        modes.push(Mode::with_interval(
            MediaFormat::srgb(FourCc::RG24, 1, 1).expect("fallback virtual format is non-zero"),
            interval_from_fps(30),
        ));
    }
    let descriptor = CaptureDescriptor::new(modes);
    let backend = ProbedBackend {
        kind: BackendKind::Virtual,
        handle: BackendHandle::Virtual,
        descriptor,
        properties: vec![("kind".into(), "virtual".into())],
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: name.to_string(),
            keys: vec!["virtual".into(), name.to_string()],
        },
        backends: vec![backend],
    }
}

/// Create a virtual RGB device with one RG24/sRGB mode.
///
/// Width, height, and fps are clamped to at least 1.
pub fn make_virtual_rgb_device(name: &str, width: u32, height: u32, fps: u32) -> ProbedDevice {
    let mode = Mode::with_interval(
        MediaFormat::srgb(FourCc::RG24, width.max(1), height.max(1))
            .expect("virtual RGB dimensions are clamped to non-zero"),
        interval_from_fps(fps),
    );
    make_virtual_device(name, [mode])
}

/// Open the first probed camera using its default backend and mode.
///
/// # Example
/// ```rust,no_run
/// use styx::prelude::*;
///
/// let handle = open_best_camera()?;
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
pub fn open_best_camera() -> Result<CaptureHandle, CaptureError> {
    let devices = crate::probe_all();
    let device = devices
        .first()
        .ok_or(CaptureError::NoCameraMatchingRequest)?;
    device.capture_request().start()
}

/// Open a virtual RGB capture source in one call.
///
/// This is useful for smoke tests, examples, and fallback pipelines.
///
/// # Example
/// ```rust
/// use styx::prelude::*;
///
/// let handle = open_virtual_rgb("virtual", 640, 360, 30)?;
/// assert_eq!(handle.mode().format.code, FourCc::RG24);
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
pub fn open_virtual_rgb(
    name: &str,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<CaptureHandle, CaptureError> {
    let device = make_virtual_rgb_device(name, width, height, fps);
    device.capture_request().start()
}

/// Create a synthetic netcam device (MJPEG over HTTP) for manual wiring.
///
/// # Example
/// ```rust,no_run
/// use styx::imports::capture::{CaptureRequest, make_netcam_device};
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
    let format = MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb);
    let mode = Mode::with_interval(format, interval);
    let descriptor = CaptureDescriptor::new([mode]);
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
/// ```rust,no_run
/// use styx::capture_api::{CaptureRequest, make_file_device};
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
            let format = MediaFormat::new(FourCc::RG24, *res, ColorSpace::Srgb);
            Mode::with_interval(format, interval)
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

    let descriptor = CaptureDescriptor::new(modes).with_controls(controls);
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
