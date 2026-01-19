#![doc = include_str!("../README.md")]

#[cfg(any(feature = "v4l2", feature = "libcamera"))]
use std::collections::HashSet;
#[cfg(feature = "v4l2")]
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(feature = "file-backend")]
use std::path::PathBuf;

pub use styx_capture as capture;
pub use styx_codec as codec;
pub use styx_core as core;
#[cfg(feature = "libcamera")]
pub use styx_libcamera as libcamera;
#[cfg(feature = "v4l2")]
pub use styx_v4l2 as v4l2;
#[cfg(feature = "preview-window")]
pub mod preview;

pub use thiserror;

pub mod capture_api;
mod metrics;
pub mod session;

/// Unified device descriptor for probed backends.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// for dev in probe_all() {
///     println!("{} backends: {}", dev.identity.display, dev.backends.len());
/// }
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ProbedDevice {
    pub identity: DeviceIdentity,
    pub backends: Vec<ProbedBackend>,
}

/// Backend-specific entry for a probed device.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let dev = probe_all().into_iter().next().expect("device");
/// for backend in dev.backends {
///     println!("{:?}: {} modes", backend.kind, backend.descriptor.modes.len());
/// }
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ProbedBackend {
    pub kind: BackendKind,
    pub handle: BackendHandle,
    pub descriptor: styx_capture::CaptureDescriptor,
    pub properties: Vec<(String, String)>,
}

/// Known backend kinds.
///
/// The `Virtual`/`Netcam`/`File` kinds map to synthetic backends created via
/// helpers in `styx::capture_api`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum BackendKind {
    V4l2,
    Libcamera,
    Virtual,
    Netcam,
    File,
}

/// Backend-specific handle used for configuration/streaming.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let dev = probe_all().into_iter().next().expect("device");
/// let handle = &dev.backends[0].handle;
/// println!("backend kind: {:?}", handle.kind());
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum BackendHandle {
    #[cfg(feature = "v4l2")]
    V4l2 {
        path: String,
    },
    #[cfg(feature = "libcamera")]
    Libcamera {
        id: String,
    },
    Virtual,
    #[cfg(feature = "netcam")]
    Netcam {
        url: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    #[cfg(feature = "file-backend")]
    File {
        #[cfg_attr(feature = "schema", schema(value_type = Vec<String>))]
        paths: Vec<PathBuf>,
        fps: u32,
        loop_forever: bool,
    },
}

impl BackendHandle {
    /// Return the backend kind for this handle.
    pub fn kind(&self) -> BackendKind {
        match self {
            #[cfg(feature = "v4l2")]
            BackendHandle::V4l2 { .. } => BackendKind::V4l2,
            #[cfg(feature = "libcamera")]
            BackendHandle::Libcamera { .. } => BackendKind::Libcamera,
            BackendHandle::Virtual => BackendKind::Virtual,
            #[cfg(feature = "netcam")]
            BackendHandle::Netcam { .. } => BackendKind::Netcam,
            #[cfg(feature = "file-backend")]
            BackendHandle::File { .. } => BackendKind::File,
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for BackendHandle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            #[derive(serde::Serialize)]
            #[serde(tag = "type", rename_all = "snake_case")]
            enum HumanHandle<'a> {
                #[cfg(feature = "v4l2")]
                V4l2 {
                    path: &'a str,
                },
                #[cfg(feature = "libcamera")]
                Libcamera {
                    id: &'a str,
                },
                Virtual,
                #[cfg(feature = "netcam")]
                Netcam {
                    url: &'a str,
                    width: u32,
                    height: u32,
                    fps: u32,
                },
                #[cfg(feature = "file-backend")]
                File {
                    paths: Vec<String>,
                    fps: u32,
                    loop_forever: bool,
                },
            }

            let human = match self {
                #[cfg(feature = "v4l2")]
                BackendHandle::V4l2 { path } => HumanHandle::V4l2 { path },
                #[cfg(feature = "libcamera")]
                BackendHandle::Libcamera { id } => HumanHandle::Libcamera { id },
                BackendHandle::Virtual => HumanHandle::Virtual,
                #[cfg(feature = "netcam")]
                BackendHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                } => HumanHandle::Netcam {
                    url,
                    width: *width,
                    height: *height,
                    fps: *fps,
                },
                #[cfg(feature = "file-backend")]
                BackendHandle::File {
                    paths,
                    fps,
                    loop_forever,
                } => HumanHandle::File {
                    paths: paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                    fps: *fps,
                    loop_forever: *loop_forever,
                },
            };
            human.serialize(serializer)
        } else {
            #[derive(serde::Serialize)]
            enum BinaryHandle<'a> {
                #[cfg(feature = "v4l2")]
                V4l2(&'a str),
                #[cfg(feature = "libcamera")]
                Libcamera(&'a str),
                Virtual,
                #[cfg(feature = "netcam")]
                Netcam {
                    url: &'a str,
                    width: u32,
                    height: u32,
                    fps: u32,
                },
                #[cfg(feature = "file-backend")]
                File {
                    paths: Vec<String>,
                    fps: u32,
                    loop_forever: bool,
                },
            }
            let bin = match self {
                #[cfg(feature = "v4l2")]
                BackendHandle::V4l2 { path } => BinaryHandle::V4l2(path),
                #[cfg(feature = "libcamera")]
                BackendHandle::Libcamera { id } => BinaryHandle::Libcamera(id),
                BackendHandle::Virtual => BinaryHandle::Virtual,
                #[cfg(feature = "netcam")]
                BackendHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                } => BinaryHandle::Netcam {
                    url,
                    width: *width,
                    height: *height,
                    fps: *fps,
                },
                #[cfg(feature = "file-backend")]
                BackendHandle::File {
                    paths,
                    fps,
                    loop_forever,
                } => BinaryHandle::File {
                    paths: paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                    fps: *fps,
                    loop_forever: *loop_forever,
                },
            };
            bin.serialize(serializer)
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for BackendHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            #[derive(serde::Deserialize)]
            #[serde(tag = "type", rename_all = "snake_case")]
            enum HumanHandle {
                #[cfg(feature = "v4l2")]
                V4l2 {
                    path: String,
                },
                #[cfg(feature = "libcamera")]
                Libcamera {
                    id: String,
                },
                Virtual,
                #[cfg(feature = "netcam")]
                Netcam {
                    url: String,
                    width: u32,
                    height: u32,
                    fps: u32,
                },
                #[cfg(feature = "file-backend")]
                File {
                    paths: Vec<String>,
                    fps: u32,
                    loop_forever: bool,
                },
            }
            let human = HumanHandle::deserialize(deserializer)?;
            let handle = match human {
                #[cfg(feature = "v4l2")]
                HumanHandle::V4l2 { path } => BackendHandle::V4l2 { path },
                #[cfg(feature = "libcamera")]
                HumanHandle::Libcamera { id } => BackendHandle::Libcamera { id },
                HumanHandle::Virtual => BackendHandle::Virtual,
                #[cfg(feature = "netcam")]
                HumanHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                } => BackendHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                },
                #[cfg(feature = "file-backend")]
                HumanHandle::File {
                    paths,
                    fps,
                    loop_forever,
                } => BackendHandle::File {
                    paths: paths.into_iter().map(PathBuf::from).collect(),
                    fps,
                    loop_forever,
                },
            };
            Ok(handle)
        } else {
            #[derive(serde::Deserialize)]
            enum BinaryHandle {
                #[cfg(feature = "v4l2")]
                V4l2(String),
                #[cfg(feature = "libcamera")]
                Libcamera(String),
                Virtual,
                #[cfg(feature = "netcam")]
                Netcam {
                    url: String,
                    width: u32,
                    height: u32,
                    fps: u32,
                },
                #[cfg(feature = "file-backend")]
                File {
                    paths: Vec<String>,
                    fps: u32,
                    loop_forever: bool,
                },
            }
            let bin = BinaryHandle::deserialize(deserializer)?;
            let handle = match bin {
                #[cfg(feature = "v4l2")]
                BinaryHandle::V4l2(path) => BackendHandle::V4l2 { path },
                #[cfg(feature = "libcamera")]
                BinaryHandle::Libcamera(id) => BackendHandle::Libcamera { id },
                BinaryHandle::Virtual => BackendHandle::Virtual,
                #[cfg(feature = "netcam")]
                BinaryHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                } => BackendHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                },
                #[cfg(feature = "file-backend")]
                BinaryHandle::File {
                    paths,
                    fps,
                    loop_forever,
                } => BackendHandle::File {
                    paths: paths.into_iter().map(PathBuf::from).collect(),
                    fps,
                    loop_forever,
                },
            };
            Ok(handle)
        }
    }
}

/// Physical device identity derived from fingerprints/props.
///
/// `display` is a human-friendly string, while `keys` contains fingerprints
/// that help merge identical devices across backends.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct DeviceIdentity {
    /// Display-friendly identifier.
    pub display: String,
    /// Fingerprint keys used for matching.
    pub keys: Vec<String>,
}

/// Probe result that includes any backend errors encountered.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let res = probe_all_with_errors();
/// for err in &res.errors {
///     eprintln!("probe error: {err}");
/// }
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProbeResult {
    pub devices: Vec<ProbedDevice>,
    pub errors: Vec<String>,
}

/// Probe all enabled backends and return a merged list.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let devices = probe_all();
/// if devices.is_empty() {
///     eprintln!("no devices found");
/// }
/// ```
pub fn probe_all() -> Vec<ProbedDevice> {
    probe_all_with_errors().devices
}

/// Probe all enabled backends and include any probe errors.
///
/// Prefer this when you want observability into backend failures.
pub fn probe_all_with_errors() -> ProbeResult {
    #[allow(unused_mut)]
    let mut devices: Vec<ProbedDevice> = Vec::new();
    #[allow(unused_mut)]
    let mut errors: Vec<String> = Vec::new();

    #[cfg(feature = "v4l2")]
    {
        let (v4l2_devices, v4l2_errors) =
            match catch_unwind(AssertUnwindSafe(|| styx_v4l2::probe_devices())) {
                Ok(res) => res,
                Err(_) => (Vec::new(), vec!["v4l2 probe panicked".to_string()]),
            };
        errors.extend(v4l2_errors);
        for dev in v4l2_devices {
            let backend = ProbedBackend {
                kind: BackendKind::V4l2,
                handle: BackendHandle::V4l2 {
                    path: dev.path.clone(),
                },
                descriptor: dev.descriptor,
                properties: dev.properties,
            };
            merge_backend(&mut devices, dev.path.clone(), backend);
        }
    }
    #[cfg(feature = "libcamera")]
    {
        for dev in styx_libcamera::probe_devices() {
            let backend = ProbedBackend {
                kind: BackendKind::Libcamera,
                handle: BackendHandle::Libcamera { id: dev.id.clone() },
                descriptor: dev.descriptor,
                properties: dev.properties,
            };
            merge_backend(&mut devices, dev.id.clone(), backend);
        }
    }
    ProbeResult { devices, errors }
}

#[cfg(any(feature = "v4l2", feature = "libcamera"))]
fn merge_backend(devices: &mut Vec<ProbedDevice>, id: String, backend: ProbedBackend) {
    let new_keys: HashSet<String> = derive_keys(&id, &backend.properties).into_iter().collect();
    let new_keys_vec: Vec<String> = new_keys.iter().cloned().collect();
    if let Some(existing) = devices
        .iter_mut()
        .find(|d| d.identity.keys.iter().any(|k| new_keys.contains(k)))
    {
        existing.backends.push(backend);
        for k in new_keys {
            if existing.identity.keys.iter().any(|ek| ek == &k) {
                continue;
            }
            existing.identity.keys.push(k);
        }
    } else {
        devices.push(ProbedDevice {
            identity: DeviceIdentity {
                display: pick_id(&id, &backend.properties),
                keys: new_keys_vec,
            },
            backends: vec![backend],
        });
    }
}

#[cfg(any(feature = "v4l2", feature = "libcamera"))]
fn derive_keys(id: &str, props: &[(String, String)]) -> Vec<String> {
    let mut keys = Vec::new();
    if !id.starts_with("/dev/video") {
        keys.push(id.to_string());
    }
    for (k, v) in props {
        let v_trimmed = v.trim();
        let v_lower = v_trimmed.to_ascii_lowercase();
        if v_lower == "rp1-cfe" {
            continue;
        }
        // Only include salient properties for matching.
        if k.eq_ignore_ascii_case("bus")
            || k.eq_ignore_ascii_case("card")
            || k.eq_ignore_ascii_case("driver")
            || k.eq_ignore_ascii_case("model")
        {
            keys.push(v_trimmed.to_string());
        }
        if let Some(vidpid) = extract_vid_pid(v_trimmed) {
            keys.push(vidpid);
        }
    }
    if let Some(vidpid) = extract_vid_pid(id) {
        keys.push(vidpid);
    }
    keys
}

#[cfg(any(feature = "v4l2", feature = "libcamera"))]
fn pick_id(id: &str, props: &[(String, String)]) -> String {
    if let Some(model) = props
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("model"))
        .map(|(_, v)| v.trim())
        && !model.is_empty()
        && !model.eq_ignore_ascii_case("rp1-cfe")
    {
        return model.to_string();
    }
    if let Some(vidpid) = props.iter().find_map(|(_, v)| extract_vid_pid(v)) {
        return vidpid;
    }
    if let Some(bus) = props
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("bus"))
        .map(|(_, v)| v.clone())
    {
        return bus;
    }
    if let Some(card) = props
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("card"))
        .map(|(_, v)| v.clone())
    {
        return card;
    }
    id.to_string()
}

#[cfg(any(feature = "v4l2", feature = "libcamera"))]
fn extract_vid_pid(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(8) {
        let slice = &bytes[i..i + 9];
        if slice[4] != b':' {
            continue;
        }
        if slice[..4].iter().all(|b| b.is_ascii_hexdigit())
            && slice[5..].iter().all(|b| b.is_ascii_hexdigit())
        {
            return Some(String::from_utf8_lossy(slice).to_string());
        }
    }
    None
}

pub mod prelude {
    #[cfg(feature = "file-backend")]
    pub use crate::capture_api::make_file_device;
    #[cfg(feature = "netcam")]
    pub use crate::capture_api::make_netcam_device;
    pub use crate::capture_api::{
        CaptureError, CaptureHandle, CaptureRequest, CaptureTunables, StyxConfig,
        set_capture_tunables, start_capture,
    };
    pub use crate::metrics::{PipelineMetrics, StageMetrics};
    #[cfg(feature = "preview-window")]
    pub use crate::preview::PreviewWindow;
    pub use crate::probe_all;
    pub use crate::session::{MediaPipeline, MediaPipelineBuilder};
    pub use styx_core::prelude::{FrameTransform, Rotation90};
    pub use crate::{BackendHandle, BackendKind, ProbedBackend, ProbedDevice};
    pub use styx_capture::prelude::*;
    pub use styx_codec::prelude::*;
    #[allow(unused_imports)]
    pub use styx_core::prelude::*;
    #[cfg(feature = "libcamera")]
    pub use styx_libcamera::prelude::{
        LibcameraCapture, LibcameraDeviceInfo, probe_devices as probe_libcamera,
    };
    #[cfg(feature = "v4l2")]
    pub use styx_v4l2::prelude::{V4l2DeviceInfo, probe_devices as probe_v4l2};
}
