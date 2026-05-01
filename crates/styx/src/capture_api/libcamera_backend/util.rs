#[cfg(feature = "v4l2")]
use std::fs;
#[cfg(feature = "v4l2")]
use std::path::Path;

use libcamera::control_value::ControlValue as LcValue;
use styx_core::controls::ControlValue;
use styx_core::prelude::*;

use crate::capture_api::{CaptureDescriptor, CaptureError, ControlApplyKind};
use crate::capture_api::{LIBCAMERA_FRAME_DURATION_LIMITS, LibcameraProcessedStreamRole};

#[cfg(feature = "v4l2")]
const V4L2_CID_VBLANK: u32 = 0x009e0901;
const CONTROL_PERMISSION_DENIED_TOKENS: &[&str] = &["permission denied"];
const CONTROL_INVALID_ARGUMENT_TOKENS: &[&str] = &["invalid argument"];
const CONTROL_SET_REJECTED_TOKENS: &[&str] = &[
    "set controls",
    "unable to set controls",
    "failed to set controls",
];
const LIBCAMERA_BUSY_TOKENS: &[&str] = &[
    "device or resource busy",
    "camera in running state",
    "resource busy",
];
const LIBCAMERA_TDN_MISMATCH_TOKENS: &[&str] = &["tdn output not enabled", "tdn enabled"];

fn bool_env_override(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| {
        let value = value.trim().to_ascii_lowercase();
        !matches!(value.as_str(), "" | "0" | "false" | "no" | "off")
    })
}

pub(super) fn stop_when_idle_enabled(configured: bool) -> bool {
    bool_env_override("STYX_LIBCAMERA_STOP_WHEN_IDLE").unwrap_or(configured)
}

pub(super) fn prefault_request_pools_enabled(configured: bool) -> bool {
    bool_env_override("STYX_LIBCAMERA_PREFAULT_REQUEST_POOLS").unwrap_or(configured)
}

pub(super) fn control_value_enabled(value: &ControlValue) -> bool {
    match value {
        ControlValue::None => false,
        ControlValue::Bool(v) => *v,
        ControlValue::Int(v) => *v != 0,
        ControlValue::Uint(v) => *v != 0,
        ControlValue::Float(v) => *v != 0.0,
    }
}

fn processed_stream_role_from_config(
    role: LibcameraProcessedStreamRole,
) -> libcamera::stream::StreamRole {
    match role {
        LibcameraProcessedStreamRole::ViewFinder => libcamera::stream::StreamRole::ViewFinder,
        LibcameraProcessedStreamRole::VideoRecording => {
            libcamera::stream::StreamRole::VideoRecording
        }
        LibcameraProcessedStreamRole::StillCapture => libcamera::stream::StreamRole::StillCapture,
    }
}

pub(super) fn processed_stream_role(
    configured: LibcameraProcessedStreamRole,
) -> libcamera::stream::StreamRole {
    let Some(value) = std::env::var("STYX_LIBCAMERA_PROCESSED_STREAM_ROLE")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
    else {
        return processed_stream_role_from_config(configured);
    };
    match value.as_str() {
        "viewfinder" | "view-finder" | "vf" => libcamera::stream::StreamRole::ViewFinder,
        "video" | "recording" | "video-recording" | "video_recording" => {
            libcamera::stream::StreamRole::VideoRecording
        }
        "still" | "still-capture" | "still_capture" => libcamera::stream::StreamRole::StillCapture,
        _ => processed_stream_role_from_config(configured),
    }
}

pub(super) fn supports_frame_duration_limits(descriptor: &CaptureDescriptor) -> bool {
    descriptor
        .controls
        .iter()
        .any(|meta| meta.id == LIBCAMERA_FRAME_DURATION_LIMITS)
}

pub(super) fn classify_libcamera_control_apply_kind(message: &str) -> ControlApplyKind {
    let msg = message.to_ascii_lowercase();
    if contains_any(&msg, CONTROL_PERMISSION_DENIED_TOKENS) {
        ControlApplyKind::PermissionDenied
    } else if contains_any(&msg, CONTROL_INVALID_ARGUMENT_TOKENS) {
        ControlApplyKind::InvalidArgument
    } else if contains_any(&msg, CONTROL_SET_REJECTED_TOKENS) {
        ControlApplyKind::SetControlsRejected
    } else {
        ControlApplyKind::Other
    }
}

pub(super) fn classify_libcamera_control_apply_message(message: impl Into<String>) -> CaptureError {
    let message = message.into();
    let kind = classify_libcamera_control_apply_kind(&message);
    CaptureError::classified_control_apply(kind, message)
}

pub(super) fn classify_libcamera_backend_message(message: impl Into<String>) -> CaptureError {
    let message = message.into();
    let msg = message.to_ascii_lowercase();
    if contains_any(&msg, LIBCAMERA_BUSY_TOKENS) {
        CaptureError::LibcameraBusy(message)
    } else if contains_any(&msg, LIBCAMERA_TDN_MISMATCH_TOKENS) {
        CaptureError::LibcameraTdnConfigurationMismatch(message)
    } else {
        CaptureError::Backend(message)
    }
}

fn contains_any(message: &str, tokens: &[&str]) -> bool {
    tokens.iter().any(|token| message.contains(token))
}

pub(super) fn from_lc_value(value: &LcValue) -> Option<ControlValue> {
    match value {
        LcValue::None => Some(ControlValue::None),
        LcValue::Bool(v) if v.len() == 1 => v.first().copied().map(ControlValue::Bool),
        LcValue::Int32(v) if v.len() == 1 => v.first().copied().map(ControlValue::Int),
        LcValue::Int64(v) if v.len() == 1 => v
            .first()
            .copied()
            .and_then(|n| i32::try_from(n).ok())
            .map(ControlValue::Int),
        LcValue::Int64(v) if v.len() == 2 => {
            let a = v.first().copied()?;
            let b = v.get(1).copied()?;
            if a == b {
                i32::try_from(a).ok().map(ControlValue::Int)
            } else {
                None
            }
        }
        LcValue::Uint16(v) if v.len() == 1 => {
            v.first().copied().map(|n| ControlValue::Uint(n as u32))
        }
        LcValue::Uint32(v) if v.len() == 1 => v.first().copied().map(ControlValue::Uint),
        LcValue::Float(v) if v.len() == 1 => v.first().copied().map(ControlValue::Float),
        _ => None,
    }
}

pub(super) fn to_lc_value(value: &ControlValue) -> Result<LcValue, CaptureError> {
    Ok(match value {
        ControlValue::None => LcValue::None,
        ControlValue::Bool(v) => LcValue::from(*v),
        ControlValue::Int(v) => LcValue::from(*v),
        ControlValue::Uint(v) => LcValue::from(*v),
        ControlValue::Float(v) => LcValue::from(*v),
    })
}

pub(super) fn stream_role_for_request(
    code: FourCc,
    configured: LibcameraProcessedStreamRole,
) -> libcamera::stream::StreamRole {
    match &code.to_u32().to_le_bytes() {
        b"H264" | b"H265" | b"HEVC" => libcamera::stream::StreamRole::VideoRecording,
        b"MJPG" | b"JPEG" => libcamera::stream::StreamRole::StillCapture,
        b"pBAA" | b"pGAA" | b"pgAA" | b"pRAA" | b"pBCC" | b"pGCC" | b"pgCC" | b"pRCC" | b"BA81"
        | b"RGGB" | b"GRBG" | b"GBRG" | b"BGGR" | b"BA10" | b"BG10" | b"GB10" | b"RG10"
        | b"BA12" | b"BG12" | b"GB12" | b"RG12" | b"BYR2" | b"R16 " | b"GREY" | b"Y10P"
        | b"Y12P" | b"Y14P" | b"Y16 " => libcamera::stream::StreamRole::Raw,
        _ => processed_stream_role(configured),
    }
}

pub(super) fn is_rpi_pisp_sensor_i2c(id: &str) -> bool {
    id.starts_with("/base/") && id.contains("/i2c@")
}

pub(super) fn pisp_disallowed_fourcc(code: FourCc) -> bool {
    matches!(
        &code.to_u32().to_le_bytes(),
        b"YV12" | b"XB24" | b"XR24" | b"YU16" | b"YV16" | b"YU24" | b"YV24" | b"YVYU" | b"VYUY"
    )
}

pub(super) fn normalize_requested_fourcc_for_libcamera(code: FourCc) -> FourCc {
    match &code.to_u32().to_le_bytes() {
        b"RG24" => FourCc::new(*b"RGB3"),
        b"BG24" => FourCc::BGR3,
        b"XR24" => FourCc::new(*b"RGB0"),
        b"XB24" => FourCc::new(*b"BGR0"),
        _ => code,
    }
}

pub(super) fn map_pixel_format_to_fourcc(pf: libcamera::pixel_format::PixelFormat) -> FourCc {
    let base = FourCc::from(pf.fourcc());
    const RGB3: [u8; 4] = *b"RGB3";
    const BGR3: [u8; 4] = *b"BGR3";
    const RGB0: [u8; 4] = *b"RGB0";
    const BGR0: [u8; 4] = *b"BGR0";
    match base.to_u32().to_le_bytes() {
        RGB3 => return FourCc::RG24,
        BGR3 => return FourCc::BG24,
        RGB0 => return FourCc::XR24,
        BGR0 => return FourCc::XB24,
        _ => {}
    }
    let Some(info) = pf.info() else {
        return base;
    };
    if !info.packed || info.colour_encoding != libcamera::pixel_format::ColourEncoding::Raw {
        return base;
    }

    const RG10: [u8; 4] = *b"RG10";
    const BG10: [u8; 4] = *b"BG10";
    const GB10: [u8; 4] = *b"GB10";
    const BA10: [u8; 4] = *b"BA10";
    const RG12: [u8; 4] = *b"RG12";
    const BG12: [u8; 4] = *b"BG12";
    const GB12: [u8; 4] = *b"GB12";
    const BA12: [u8; 4] = *b"BA12";

    match (base.to_u32().to_le_bytes(), info.bits_per_pixel) {
        (RG10, 10) => FourCc::new(*b"pRAA"),
        (BG10, 10) => FourCc::new(*b"pBAA"),
        (GB10, 10) => FourCc::new(*b"pGAA"),
        (BA10, 10) => FourCc::new(*b"pgAA"),
        (RG12, 12) => FourCc::new(*b"pRCC"),
        (BG12, 12) => FourCc::new(*b"pBCC"),
        (GB12, 12) => FourCc::new(*b"pGCC"),
        (BA12, 12) => FourCc::new(*b"pgCC"),
        _ => base,
    }
}

pub(super) fn plane_height_for_format(code: FourCc, plane_idx: usize, height: usize) -> usize {
    const NV12: FourCc = FourCc::NV12;
    const I420: FourCc = FourCc::I420;
    const YU12: FourCc = FourCc::YU12;
    const YV12: FourCc = FourCc::YV12;

    if code == NV12 {
        return if plane_idx == 0 { height } else { height / 2 };
    }
    if code == I420 || code == YU12 || code == YV12 {
        return if plane_idx == 0 { height } else { height / 2 };
    }
    height
}

#[cfg(feature = "v4l2")]
fn find_sensor_subdev_for_libcamera_id(id: &str) -> Option<String> {
    fn sensor_name_from_id(id: &str) -> Option<&str> {
        let last = id.rsplit('/').next()?;
        Some(last.split('@').next().unwrap_or(last))
    }

    fn canonical_dt_path(of_node: &Path) -> Option<String> {
        let Ok(target) = fs::canonicalize(of_node) else {
            return None;
        };
        let target = target.to_string_lossy();
        target
            .strip_prefix("/sys/firmware/devicetree")
            .or_else(|| target.strip_prefix("/proc/device-tree"))
            .map(|s| s.to_string())
    }

    let sys = Path::new("/sys/class/video4linux");
    let Ok(entries) = fs::read_dir(sys) else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("v4l-subdev") {
            continue;
        }
        let dt_path = canonical_dt_path(&entry.path().join("device/of_node"));
        if dt_path.as_deref() != Some(id) {
            continue;
        }
        return Some(format!("/dev/{name}"));
    }

    let sensor = sensor_name_from_id(id)?;
    let Ok(entries) = fs::read_dir(sys) else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("v4l-subdev") {
            continue;
        }
        let Ok(dev_name) = fs::read_to_string(entry.path().join("name")) else {
            continue;
        };
        let dev_name = dev_name.trim();
        if dev_name.starts_with(sensor) {
            return Some(format!("/dev/{name}"));
        }
    }
    None
}

#[cfg(feature = "v4l2")]
pub(super) fn try_set_sensor_vblank_min_for_high_fps(id: &str) {
    let Some(path) = find_sensor_subdev_for_libcamera_id(id) else {
        return;
    };
    let Ok(dev) = v4l::Device::with_path(&path) else {
        return;
    };
    let Ok(descs) = dev.query_controls() else {
        return;
    };
    let Some(vblank) = descs.iter().find(|d| d.id == V4L2_CID_VBLANK) else {
        return;
    };
    let min = vblank.minimum;
    let _ = dev.set_control(v4l::control::Control {
        id: V4L2_CID_VBLANK,
        value: v4l::control::Value::Integer(min),
    });
}
