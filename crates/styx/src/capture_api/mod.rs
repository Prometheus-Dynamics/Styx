//! Capture helpers, request builders, and backend constructors.
//!
//! Most users will interact with `CaptureRequest` or `MediaPipelineBuilder`.
//!
//! # Example
//! ```rust,no_run
//! use styx::prelude::*;
//!
//! let device = make_virtual_rgb_device("virtual", 640, 360, 30);
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
#[cfg(feature = "simulation-bevy")]
pub(super) mod simulation_backend;
#[cfg(feature = "v4l2")]
pub(super) mod v4l2_backend;
pub(super) mod virtual_backend;

pub use control_plane::ControlPlane;
#[cfg(feature = "graph-pipeline")]
pub(crate) use control_plane::{apply_control_to_plane, read_control_from_plane};
pub use handle::{CaptureFrameIter, CaptureHandle, WorkerHandle};
pub use request::{
    CameraFormat, CameraIntervalPreference, CameraRequest, CameraStartPolicy, CaptureError,
    CaptureRequest, CaptureStartPolicy, ControlApplyKind, SelectedCamera, TdnOutputMode,
    start_capture,
};
pub use tunables::{
    BackendConfig, CaptureConfig, CaptureTunables, DEFAULT_CAPTURE_QUEUE_SEND_TIMEOUT_MS,
    DEFAULT_LIBCAMERA_CONTROL_RESPONSE_TIMEOUT_MS, DEFAULT_LIBCAMERA_IDLE_DRAIN_POLL_MS,
    DEFAULT_LIBCAMERA_IDLE_DRAIN_TIMEOUT_MS, DEFAULT_LIBCAMERA_LOOKUP_POLL_MS,
    DEFAULT_LIBCAMERA_LOOKUP_TIMEOUT_MS, DEFAULT_LIBCAMERA_PREFAULT_REQUEST_POOLS,
    DEFAULT_LIBCAMERA_PROBE_CACHE_MS, DEFAULT_LIBCAMERA_REQUEST_POLL_MS,
    DEFAULT_LIBCAMERA_REQUEUE_STALL_TIMEOUT_MS, DEFAULT_LIBCAMERA_STOP_WHEN_IDLE,
    DEFAULT_NETCAM_BACKOFF_MAX_MS, DEFAULT_NETCAM_BACKOFF_START_MS, DEFAULT_NETCAM_SEND_TIMEOUT_MS,
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
#[cfg(feature = "simulation-bevy")]
use std::path::PathBuf;
use styx_capture::prelude::*;

mod handle;
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

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SimulationPose {
    pub translation_m: [f32; 3],
    pub rotation_deg: [f32; 3],
}

impl Default for SimulationPose {
    fn default() -> Self {
        Self {
            translation_m: [0.0, 0.0, 3.0],
            rotation_deg: [0.0, 0.0, 0.0],
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SimulationSensorConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub sensor_width_mm: f32,
    pub sensor_height_mm: f32,
    pub near_m: f32,
    pub far_m: f32,
}

impl Default for SimulationSensorConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            sensor_width_mm: 36.0,
            sensor_height_mm: 24.0,
            near_m: 0.05,
            far_m: 2_000.0,
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SimulationLensConfig {
    pub focal_length_mm: f32,
    pub aperture_f_stop: f32,
    pub focus_distance_m: f32,
}

impl Default for SimulationLensConfig {
    fn default() -> Self {
        Self {
            focal_length_mm: 35.0,
            aperture_f_stop: 2.8,
            focus_distance_m: 5.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum SimulationOutputMode {
    #[default]
    Rgb,
    Depth,
    Normals,
    Segmentation,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SimulationDeviceConfig {
    pub sensor: SimulationSensorConfig,
    pub lens: SimulationLensConfig,
    pub pose: SimulationPose,
    pub output_mode: SimulationOutputMode,
    pub clear_color_rgba: [f32; 4],
}

impl Default for SimulationDeviceConfig {
    fn default() -> Self {
        Self {
            sensor: SimulationSensorConfig::default(),
            lens: SimulationLensConfig::default(),
            pose: SimulationPose::default(),
            output_mode: SimulationOutputMode::Rgb,
            clear_color_rgba: [0.03, 0.04, 0.05, 1.0],
        }
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
/// let device = make_virtual_rgb_device("virtual", 640, 360, 30);
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

/// Create a synthetic simulation device that loads a scene file into a Bevy world.
///
/// Scene ingest and camera/sensor controls are exposed through the same capture
/// API as physical and file-backed sources.
#[cfg(feature = "simulation-bevy")]
pub fn make_simulation_device(
    name: &str,
    scene_path: PathBuf,
    config: SimulationDeviceConfig,
) -> ProbedDevice {
    let res = Resolution::new(config.sensor.width.max(1), config.sensor.height.max(1))
        .unwrap_or_else(|| Resolution::new(1, 1).unwrap());
    let interval = interval_from_fps(config.sensor.fps.max(1));
    let format = match config.output_mode {
        SimulationOutputMode::Depth => MediaFormat::new(FourCc::D32F, res, ColorSpace::Unknown),
        _ => MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb),
    };
    let mode = Mode::with_interval(format, interval);

    let controls = vec![
        ControlMeta {
            id: simulation_backend::control_id_output_mode(),
            name: "simulation.output.mode".into(),
            kind: ControlKind::Menu,
            access: Access::ReadWrite,
            min: ControlValue::Uint(0),
            max: ControlValue::Uint(3),
            default: ControlValue::Uint(match config.output_mode {
                SimulationOutputMode::Rgb => 0,
                SimulationOutputMode::Depth => 1,
                SimulationOutputMode::Normals => 2,
                SimulationOutputMode::Segmentation => 3,
            }),
            step: Some(ControlValue::Uint(1)),
            menu: Some(vec![
                "rgb".into(),
                "depth".into(),
                "normals".into(),
                "segmentation".into(),
            ]),
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_translation_x(),
            name: "simulation.sensor.translation_x_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-10_000.0),
            max: ControlValue::Float(10_000.0),
            default: ControlValue::Float(config.pose.translation_m[0]),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_translation_y(),
            name: "simulation.sensor.translation_y_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-10_000.0),
            max: ControlValue::Float(10_000.0),
            default: ControlValue::Float(config.pose.translation_m[1]),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_translation_z(),
            name: "simulation.sensor.translation_z_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-10_000.0),
            max: ControlValue::Float(10_000.0),
            default: ControlValue::Float(config.pose.translation_m[2]),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_rotation_roll(),
            name: "simulation.sensor.rotation_roll_deg".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-360.0),
            max: ControlValue::Float(360.0),
            default: ControlValue::Float(config.pose.rotation_deg[0]),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_rotation_pitch(),
            name: "simulation.sensor.rotation_pitch_deg".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-360.0),
            max: ControlValue::Float(360.0),
            default: ControlValue::Float(config.pose.rotation_deg[1]),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_rotation_yaw(),
            name: "simulation.sensor.rotation_yaw_deg".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-360.0),
            max: ControlValue::Float(360.0),
            default: ControlValue::Float(config.pose.rotation_deg[2]),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_focal_length(),
            name: "simulation.lens.focal_length_mm".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(1.0),
            max: ControlValue::Float(5_000.0),
            default: ControlValue::Float(config.lens.focal_length_mm),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_aperture_f_stop(),
            name: "simulation.lens.aperture_f_stop".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.7),
            max: ControlValue::Float(64.0),
            default: ControlValue::Float(config.lens.aperture_f_stop),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_focus_distance(),
            name: "simulation.lens.focus_distance_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.01),
            max: ControlValue::Float(100_000.0),
            default: ControlValue::Float(config.lens.focus_distance_m),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_sensor_width(),
            name: "simulation.sensor.width_mm".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.1),
            max: ControlValue::Float(1_000.0),
            default: ControlValue::Float(config.sensor.sensor_width_mm),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_sensor_height(),
            name: "simulation.sensor.height_mm".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.1),
            max: ControlValue::Float(1_000.0),
            default: ControlValue::Float(config.sensor.sensor_height_mm),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_near_plane(),
            name: "simulation.sensor.near_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.001),
            max: ControlValue::Float(100.0),
            default: ControlValue::Float(config.sensor.near_m),
            step: Some(ControlValue::Float(0.001)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: simulation_backend::control_id_far_plane(),
            name: "simulation.sensor.far_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.01),
            max: ControlValue::Float(1_000_000.0),
            default: ControlValue::Float(config.sensor.far_m),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
    ];

    let descriptor = CaptureDescriptor::new([mode]).with_controls(controls);
    let backend = ProbedBackend {
        kind: BackendKind::Simulation,
        handle: BackendHandle::Simulation {
            scene_path: scene_path.clone(),
            config: config.clone(),
        },
        descriptor,
        properties: vec![(
            "scene_path".into(),
            scene_path.to_string_lossy().to_string(),
        )],
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: name.to_string(),
            keys: vec![scene_path.to_string_lossy().to_string()],
        },
        backends: vec![backend],
    }
}
