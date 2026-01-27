use std::collections::{HashMap, HashSet};
#[cfg(feature = "v4l2")]
use std::fs;
#[cfg(feature = "v4l2")]
use std::path::Path;
use std::thread;
use std::time::Duration;

use libcamera::framebuffer::AsFrameBuffer;
use libcamera::framebuffer_allocator::FrameBuffer;
use libcamera::request::Request;
use libcamera::request::ReuseFlag;
use libcamera::{control::ControlList as LcControlList, control_value::ControlValue as LcValue};
use std::sync::atomic::{AtomicBool, Ordering};
use styx_codec::Codec;
use styx_codec::prelude::{Nv12ToBgrDecoder, Nv12ToRgbDecoder, YuyvToRgbDecoder};
use styx_core::prelude::*;

use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, TdnOutputMode, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode, ModeId};
use crate::{BackendHandle, BackendKind, ProbedBackend};

#[cfg(feature = "v4l2")]
const V4L2_CID_VBLANK: u32 = 0x009e0901;
const LIBCAMERA_FRAME_DURATION_LIMITS: ControlId = ControlId(30);

#[derive(Clone, Copy, Debug)]
struct LibcameraFpsControls {
    ae_enable: Option<ControlId>,
    exposure_time: Option<ControlId>,
}

fn find_control_id(descriptor: &CaptureDescriptor, name: &str) -> Option<ControlId> {
    descriptor
        .controls
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.id)
}

fn find_control_meta<'a>(
    descriptor: &'a CaptureDescriptor,
    id: ControlId,
) -> Option<&'a ControlMeta> {
    descriptor.controls.iter().find(|c| c.id == id)
}

fn cv_i32(value: &ControlValue) -> Option<i32> {
    match value {
        ControlValue::Int(v) => Some(*v),
        ControlValue::Uint(v) => (*v).try_into().ok(),
        _ => None,
    }
}

fn control_value_enabled(value: &ControlValue) -> bool {
    match value {
        ControlValue::None => false,
        ControlValue::Bool(v) => *v,
        ControlValue::Int(v) => *v != 0,
        ControlValue::Uint(v) => *v != 0,
        ControlValue::Float(v) => *v != 0.0,
    }
}

fn supports_frame_duration_limits(descriptor: &CaptureDescriptor) -> bool {
    descriptor
        .controls
        .iter()
        .any(|meta| meta.id == LIBCAMERA_FRAME_DURATION_LIMITS)
}

fn is_control_apply_error(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("set controls")
        || msg.contains("unable to set controls")
        || msg.contains("failed to set controls")
        || msg.contains("permission denied")
        || msg.contains("invalid argument")
}

fn from_lc_value(value: &LcValue) -> Option<ControlValue> {
    match value {
        LcValue::None => Some(ControlValue::None),
        LcValue::Bool(v) if v.len() == 1 => v.get(0).copied().map(ControlValue::Bool),
        LcValue::Int32(v) if v.len() == 1 => v.get(0).copied().map(ControlValue::Int),
        LcValue::Int64(v) if v.len() == 1 => v.get(0).copied().and_then(|n| i32::try_from(n).ok()).map(ControlValue::Int),
        LcValue::Int64(v) if v.len() == 2 => {
            let a = v.get(0).copied()?;
            let b = v.get(1).copied()?;
            if a == b {
                i32::try_from(a).ok().map(ControlValue::Int)
            } else {
                None
            }
        }
        LcValue::Uint16(v) if v.len() == 1 => v.get(0).copied().map(|n| ControlValue::Uint(n as u32)),
        LcValue::Uint32(v) if v.len() == 1 => v.get(0).copied().map(ControlValue::Uint),
        LcValue::Float(v) if v.len() == 1 => v.get(0).copied().map(ControlValue::Float),
        _ => None,
    }
}

fn stream_role_for_request(code: FourCc) -> libcamera::stream::StreamRole {
    match &code.to_u32().to_le_bytes() {
        // Encoded video streams.
        b"H264" | b"H265" | b"HEVC" => libcamera::stream::StreamRole::VideoRecording,
        // Encoded stills / MJPEG.
        b"MJPG" | b"JPEG" => libcamera::stream::StreamRole::StillCapture,
        // Raw Bayer (packed or unpacked) + raw mono.
        b"pBAA" | b"pGAA" | b"pgAA" | b"pRAA" | b"pBCC" | b"pGCC" | b"pgCC" | b"pRCC" | b"BA81"
        | b"RGGB" | b"GRBG" | b"GBRG" | b"BGGR" | b"BA10" | b"BG10" | b"GB10" | b"RG10"
        | b"BA12" | b"BG12" | b"GB12" | b"RG12" | b"BYR2" | b"R16 " | b"GREY" | b"Y10P"
        | b"Y12P" | b"Y14P" | b"Y16 " => libcamera::stream::StreamRole::Raw,
        // ISP-processed formats (NV12/RGB/etc) typically come from ViewFinder on PiSP.
        _ => libcamera::stream::StreamRole::ViewFinder,
    }
}

fn is_rpi_pisp_sensor_i2c(id: &str) -> bool {
    // PiSP libcamera IDs for DT cameras are usually device-tree paths under /base/... and
    // sensors are on rp1 I2C.
    id.starts_with("/base/") && id.contains("/i2c@")
}

fn pisp_disallowed_fourcc(code: FourCc) -> bool {
    // PiSP asserts on several formats during configuration validation.
    matches!(
        &code.to_u32().to_le_bytes(),
        b"YV12" | b"XB24" | b"XR24" | b"YU16" | b"YV16" | b"YU24" | b"YV24" | b"YVYU" | b"VYUY"
    )
}

/// Map internal "friendly" FourCC aliases to libcamera/V4L2 FourCCs.
///
/// `RG24` is used throughout Styx/HeliOS as "packed RGB24", but libcamera expects `RGB3`.
fn normalize_requested_fourcc_for_libcamera(code: FourCc) -> FourCc {
    match &code.to_u32().to_le_bytes() {
        b"RG24" => FourCc::new(*b"RGB3"),
        b"BG24" => FourCc::new(*b"BGR3"),
        // Treat these as XRGB/XBGR (alpha/unused byte) where supported.
        b"XR24" => FourCc::new(*b"RGB0"),
        b"XB24" => FourCc::new(*b"BGR0"),
        _ => code,
    }
}

fn map_pixel_format_to_fourcc(pf: libcamera::pixel_format::PixelFormat) -> FourCc {
    let base = FourCc::from(pf.fourcc());
    const RGB3: [u8; 4] = *b"RGB3";
    const BGR3: [u8; 4] = *b"BGR3";
    const RGB0: [u8; 4] = *b"RGB0";
    const BGR0: [u8; 4] = *b"BGR0";
    match base.to_u32().to_le_bytes() {
        // Normalize libcamera's RGB/BGR FourCCs into Styx's "friendly" aliases.
        RGB3 => return FourCc::new(*b"RG24"),
        BGR3 => return FourCc::new(*b"BG24"),
        RGB0 => return FourCc::new(*b"XR24"),
        BGR0 => return FourCc::new(*b"XB24"),
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
        // RAW10 MIPI packed.
        (RG10, 10) => FourCc::new(*b"pRAA"),
        (BG10, 10) => FourCc::new(*b"pBAA"),
        (GB10, 10) => FourCc::new(*b"pGAA"),
        (BA10, 10) => FourCc::new(*b"pgAA"),

        // RAW12 MIPI packed.
        (RG12, 12) => FourCc::new(*b"pRCC"),
        (BG12, 12) => FourCc::new(*b"pBCC"),
        (GB12, 12) => FourCc::new(*b"pGCC"),
        (BA12, 12) => FourCc::new(*b"pgCC"),

        _ => base,
    }
}

fn plane_height_for_format(code: FourCc, plane_idx: usize, height: usize) -> usize {
    const NV12: FourCc = FourCc::new(*b"NV12");
    const I420: FourCc = FourCc::new(*b"I420");
    const YU12: FourCc = FourCc::new(*b"YU12");
    const YV12: FourCc = FourCc::new(*b"YV12");

    if code == NV12 {
        return if plane_idx == 0 { height } else { height / 2 };
    }

    if code == I420 || code == YU12 || code == YV12 {
        return if plane_idx == 0 { height } else { height / 2 };
    }

    height
}

fn infer_stride(bytes_used: usize, plane_len: usize, plane_height: usize) -> usize {
    if plane_height == 0 {
        return bytes_used.max(plane_len);
    }
    let by_used = if bytes_used > 0 {
        bytes_used
    } else {
        plane_len
    };
    let mut stride = by_used / plane_height;
    if stride == 0 {
        stride = 1;
    }
    // Clamp stride to the maximum representable by the mapped plane slice.
    let max_stride = plane_len / plane_height;
    if max_stride > 0 {
        stride = stride.min(max_stride);
    }
    stride
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
        // On Linux, device-tree is typically exposed at /sys/firmware/devicetree or /proc/device-tree.
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

    // Fallback: match by the kernel-reported subdev name (e.g. "ov9782 10-0060").
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
fn try_set_sensor_vblank_min_for_high_fps(id: &str) {
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

pub(super) fn start_libcamera(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
    tdn_output_mode: TdnOutputMode,
) -> Result<CaptureHandle, CaptureError> {
    use libcamera::camera::CameraConfigurationStatus;
    use libcamera::framebuffer_map::MemoryMappedFrameBuffer;
    use libcamera::geometry::Size;
    use std::sync::mpsc;
    use std::sync::mpsc::RecvTimeoutError;

    let id = match &backend.handle {
        BackendHandle::Libcamera { id } => id.clone(),
        _ => return Err(CaptureError::Backend("libcamera id missing".into())),
    };
    let writable_controls: HashSet<ControlId> = descriptor
        .controls
        .iter()
        .filter(|c| matches!(c.access, Access::ReadWrite))
        .map(|c| c.id)
        .collect();
    let requested_controls: Vec<(ControlId, ControlValue)> = controls
        .into_iter()
        .filter(|(id, _)| writable_controls.contains(id))
        .collect();
    let requires_tdn_output = requested_controls.iter().any(|(id, value)| {
        if !control_value_enabled(value) {
            return false;
        }
        descriptor
            .controls
            .iter()
            .find(|meta| meta.id == *id)
            .is_some_and(|meta| meta.metadata.requires_tdn_output)
    });
    let enable_tdn_output = match tdn_output_mode {
        TdnOutputMode::Off => false,
        TdnOutputMode::Auto => requires_tdn_output,
        TdnOutputMode::Force => true,
    };
    if enable_tdn_output && !is_rpi_pisp_sensor_i2c(&id) {
        return Err(CaptureError::InvalidConfig(
            "tdn output not supported for this device".into(),
        ));
    }
    if requires_tdn_output && !enable_tdn_output {
        return Err(CaptureError::InvalidConfig(
            "tdn output required by requested controls".into(),
        ));
    }

    let fps_controls = LibcameraFpsControls {
        ae_enable: find_control_id(&descriptor, "AeEnable")
            .filter(|id| writable_controls.contains(id)),
        exposure_time: find_control_id(&descriptor, "ExposureTime")
            .filter(|id| writable_controls.contains(id)),
    };
    let requested_control_ids: HashSet<ControlId> =
        requested_controls.iter().map(|(id, _)| *id).collect();
    let supports_frame_duration = supports_frame_duration_limits(&descriptor);

    let enable_tdn_output_for_thread = enable_tdn_output;
    let id_for_thread = id.clone();
    let writable_controls_for_thread = writable_controls.clone();
    let requested_controls_for_thread = requested_controls.clone();
    let interval_for_thread = interval;
    let fps_controls_for_thread = fps_controls;
    let requested_control_ids_for_thread = requested_control_ids;
    let descriptor_for_thread = descriptor.clone();
    let supports_frame_duration_for_thread = supports_frame_duration;

    let requested_fps = interval
        .map(|i| i.denominator.get() as f64 / i.numerator.get().max(1) as f64)
        .unwrap_or(0.0);
    let queue_depth = crate::capture_api::capture_queue_depth();
    // High-FPS capture can benefit from extra in-flight buffers, but that comes with a large
    // memory cost at full resolution (especially on PiSP). Keep buffer depth strictly user-tuned
    // via `CaptureTunables` instead of forcing a higher default here.
    let _ = requested_fps;
    let (tx, rx) = bounded(queue_depth);
    let (setup_tx, setup_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let (ctrl_tx, ctrl_rx) = mpsc::channel();
    let pending_controls = std::sync::Arc::new(std::sync::Mutex::new(PendingControlState::default()));
    let mode_for_thread = mode.clone();

    let pending_controls_for_thread = pending_controls.clone();
    let worker = thread::spawn(move || {
        let res: Result<Mode, CaptureError> = (|| {
            let shutting_down = std::sync::Arc::new(AtomicBool::new(false));
            let _shutdown_guard = ShutdownGuard(shutting_down.clone());
            let mgr = styx_libcamera::manager().map_err(|e| CaptureError::Backend(e))?;
            let cameras = mgr.cameras();
            let cam = (0..cameras.len()).find_map(|idx| {
                let cam = cameras.get(idx)?;
                if cam.id() == id_for_thread {
                    Some(cam)
                } else {
                    None
                }
            });
            let mut cam = cam
                .ok_or_else(|| CaptureError::Backend("camera not found".into()))?
                .acquire()
                .map_err(|e| CaptureError::Backend(e.to_string()))?;

            let role = stream_role_for_request(mode_for_thread.format.code);
            let enable_tdn_output = enable_tdn_output_for_thread;
            let mut roles = vec![role];
            if enable_tdn_output {
                roles.push(libcamera::stream::StreamRole::VideoRecording);
            }
            let mut cfgs = cam
                .generate_configuration(&roles)
                .ok_or_else(|| CaptureError::Backend("generate_configuration failed".into()))?;
            if enable_tdn_output && cfgs.get(1).is_none() {
                return Err(CaptureError::Backend(
                    "tdn output stream unavailable".into(),
                ));
            }
            let requested_code = mode_for_thread.format.code;
            if is_rpi_pisp_sensor_i2c(&id_for_thread) && pisp_disallowed_fourcc(requested_code) {
                return Err(CaptureError::Backend(format!(
                    "{} unsupported on PiSP",
                    requested_code
                )));
            }
            let libcamera_code = normalize_requested_fourcc_for_libcamera(requested_code);
            let is_rgb24_request = matches!(&libcamera_code.to_u32().to_le_bytes(), b"RGB3" | b"BGR3");
            let emulate_rgb24 = is_rgb24_request && is_rpi_pisp_sensor_i2c(&id_for_thread);

            // PiSP (rpi/pisp) currently asserts/crashes in libcamera when validating sensor-camera
            // configs that request RGB24/BGR24. To keep the API true to the requested format, we
            // capture YUV (NV12 preferred) and convert to the requested RGB/BGR in software.
            {
                let depth_u32 = u32::try_from(queue_depth).unwrap_or(4).clamp(4, 12);
                let mut cfg = cfgs
                    .get_mut(0)
                    .ok_or_else(|| CaptureError::Backend("missing stream config".into()))?;
                let desired_format = if emulate_rgb24 { FourCc::new(*b"NV12") } else { libcamera_code };
                cfg.set_pixel_format(libcamera::pixel_format::PixelFormat::new(desired_format.to_u32(), 0));
                cfg.set_size(Size::new(
                    mode_for_thread.format.resolution.width.get(),
                    mode_for_thread.format.resolution.height.get(),
                ));
                cfg.set_buffer_count(depth_u32);

                if enable_tdn_output {
                    if let Some(mut tdn_cfg) = cfgs.get_mut(1) {
                        tdn_cfg.set_pixel_format(libcamera::pixel_format::PixelFormat::new(desired_format.to_u32(), 0));
                        tdn_cfg.set_size(Size::new(
                            mode_for_thread.format.resolution.width.get(),
                            mode_for_thread.format.resolution.height.get(),
                        ));
                        tdn_cfg.set_buffer_count(depth_u32);
                    }
                }
            }
            if matches!(cfgs.validate(), CameraConfigurationStatus::Invalid) {
                if emulate_rgb24 {
                    {
                        let mut cfg = cfgs
                            .get_mut(0)
                            .ok_or_else(|| CaptureError::Backend("missing stream config".into()))?;
                        cfg.set_pixel_format(libcamera::pixel_format::PixelFormat::new(
                            FourCc::new(*b"YUYV").to_u32(),
                            0,
                        ));
                    }
                    if enable_tdn_output {
                        if let Some(mut tdn_cfg) = cfgs.get_mut(1) {
                            tdn_cfg.set_pixel_format(libcamera::pixel_format::PixelFormat::new(
                                FourCc::new(*b"YUYV").to_u32(),
                                0,
                            ));
                        }
                    }
                    if matches!(cfgs.validate(), CameraConfigurationStatus::Invalid) {
                        return Err(CaptureError::Backend("config invalid".into()));
                    }
                } else {
                    return Err(CaptureError::Backend("config invalid".into()));
                }
            }
            cam.configure(&mut cfgs)
                .map_err(|e| CaptureError::Backend(e.to_string()))?;

            if let Some(interval) = interval_for_thread {
                let num = interval.numerator.get() as f64;
                let den = interval.denominator.get() as f64;
                let fps = if num > 0.0 { den / num } else { 0.0 };
                if fps >= 60.0 {
                    #[cfg(feature = "v4l2")]
                    try_set_sensor_vblank_min_for_high_fps(&id_for_thread);
                }
            }

            let cfg = cfgs
                .get(0)
                .ok_or_else(|| CaptureError::Backend("missing validated config".into()))?;
            let validated_pix = cfg.get_pixel_format();
            let validated_size = cfg.get_size();
            let validated_res = Resolution::new(validated_size.width, validated_size.height)
                .unwrap_or(mode_for_thread.format.resolution);
            let validated_code = map_pixel_format_to_fourcc(validated_pix);
            let wire_format = MediaFormat::new(validated_code, validated_res, mode_for_thread.format.color);
            let output_format = if emulate_rgb24 {
                MediaFormat::new(requested_code, validated_res, mode_for_thread.format.color)
            } else {
                wire_format
            };
            let validated_mode = Mode {
                id: ModeId {
                    format: output_format,
                    interval: mode_for_thread.id.interval,
                },
                format: output_format,
                intervals: mode_for_thread.intervals.clone(),
                interval_stepwise: mode_for_thread.interval_stepwise,
            };

            enum Emulation {
                Nv12ToRgb(Nv12ToRgbDecoder),
                Nv12ToBgr(Nv12ToBgrDecoder),
                YuyvToRgb(YuyvToRgbDecoder),
            }

            let emulation: Option<Emulation> = if emulate_rgb24 {
                match (
                    &validated_code.to_u32().to_le_bytes(),
                    &requested_code.to_u32().to_le_bytes(),
                ) {
                    (b"NV12", b"RG24") => Some(Emulation::Nv12ToRgb(Nv12ToRgbDecoder::new(
                        validated_res.width.get(),
                        validated_res.height.get(),
                    ))),
                    (b"NV12", b"BG24") => Some(Emulation::Nv12ToBgr(Nv12ToBgrDecoder::new(
                        validated_res.width.get(),
                        validated_res.height.get(),
                    ))),
                    (b"YUYV", b"RG24") => Some(Emulation::YuyvToRgb(YuyvToRgbDecoder::new(
                        validated_res.width.get(),
                        validated_res.height.get(),
                    ))),
                    _ => None,
                }
            } else {
                None
            };
            let stream = cfg
                .stream()
                .ok_or_else(|| CaptureError::Backend("missing stream".into()))?;
            let tdn_stream = if enable_tdn_output {
                cfgs.get(1).and_then(|cfg| cfg.stream())
            } else {
                None
            };
            let cfg_stride = cfg.get_stride() as usize;
            let tdn_stride = if enable_tdn_output {
                cfgs.get(1).map(|cfg| cfg.get_stride() as usize)
            } else {
                None
            };
            let mut alloc = libcamera::framebuffer_allocator::FrameBufferAllocator::new(&cam);
            let bufs = alloc
                .alloc(&stream)
                .map_err(|e| CaptureError::Backend(e.to_string()))?;
            let tdn_bufs = if let Some(tdn_stream) = &tdn_stream {
                Some(
                    alloc
                        .alloc(tdn_stream)
                        .map_err(|e| CaptureError::Backend(e.to_string()))?,
                )
            } else {
                None
            };
            let mapped = bufs
                .into_iter()
                .map(|buf| {
                    MemoryMappedFrameBuffer::new(buf)
                        .map_err(|e| CaptureError::Backend(e.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mapped_tdn = if let Some(tdn_bufs) = tdn_bufs {
                Some(
                    tdn_bufs
                        .into_iter()
                        .map(|buf| {
                            MemoryMappedFrameBuffer::new(buf)
                                .map_err(|e| CaptureError::Backend(e.to_string()))
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                )
            } else {
                None
            };

            let mut requests = Vec::new();
            if let Some(tdn_stream) = &tdn_stream {
                let Some(mapped_tdn) = mapped_tdn else {
                    return Err(CaptureError::Backend("tdn output buffers unavailable".into()));
                };
                if mapped_tdn.is_empty() {
                    return Err(CaptureError::Backend("tdn output buffers unavailable".into()));
                }
                for ((i, buf), tdn_buf) in mapped.into_iter().enumerate().zip(mapped_tdn.into_iter()) {
                    let mut req = cam
                        .create_request(Some(i as u64))
                        .ok_or_else(|| CaptureError::Backend("request create failed".into()))?;
                    req.add_buffer(&stream, buf)
                        .map_err(|e| CaptureError::Backend(e.to_string()))?;
                    req.add_buffer(tdn_stream, tdn_buf)
                        .map_err(|e| CaptureError::Backend(e.to_string()))?;
                    requests.push(req);
                }
            } else {
                for (i, buf) in mapped.into_iter().enumerate() {
                    let mut req = cam
                        .create_request(Some(i as u64))
                        .ok_or_else(|| CaptureError::Backend("request create failed".into()))?;
                    req.add_buffer(&stream, buf)
                        .map_err(|e| CaptureError::Backend(e.to_string()))?;
                    requests.push(req);
                }
            }

            let ctrl_list = build_libcamera_controls(&requested_controls_for_thread)?;
            let mut ctrl_list = ctrl_list;
            let mut frame_duration: Option<i64> = None;
            if let Some(interval) = interval_for_thread
                && supports_frame_duration_for_thread
            {
                // libcamera expects frame duration limits in microseconds (min/max).
                // Our Interval follows the V4L2 convention of "seconds per frame" (numerator/denominator).
                let num = interval.numerator.get() as u64;
                let den = interval.denominator.get() as u64;
                let duration_us = num.saturating_mul(1_000_000).saturating_div(den.max(1));
                let duration = duration_us.clamp(1, i64::MAX as u64) as i64;
                frame_duration = Some(duration);
                // Control id 30 is FrameDurationLimits in libcamera.
                let _ = ctrl_list
                    .set_raw(30, LcValue::from([duration, duration]))
                    .map_err(|e| CaptureError::ControlApply(e.to_string()))?;

                // For high FPS, keep exposure <= frame duration and consider disabling AE.
                if let Some(ae_id) = fps_controls_for_thread.ae_enable
                    && !requested_control_ids_for_thread.contains(&ae_id)
                {
                    let _ = ctrl_list
                        .set_raw(ae_id.0, LcValue::from(false))
                        .map_err(|e| CaptureError::ControlApply(e.to_string()))?;
                }

                if let Some(exposure_id) = fps_controls_for_thread.exposure_time
                    && !requested_control_ids_for_thread.contains(&exposure_id)
                {
                    let margin_us: i64 = 200;
                    let mut desired = (duration - margin_us).max(1);
                    if let Some(meta) = find_control_meta(&descriptor_for_thread, exposure_id) {
                        if let Some(min) = cv_i32(&meta.min) {
                            desired = desired.max(min as i64);
                        }
                        if let Some(max) = cv_i32(&meta.max) {
                            desired = desired.min(max as i64);
                        }
                    }
                    let desired_i32 = desired.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                    let _ = ctrl_list
                        .set_raw(exposure_id.0, LcValue::from(desired_i32))
                        .map_err(|e| CaptureError::ControlApply(e.to_string()))?;
                }
            }
            let mut start_ctrls = if ctrl_list.is_empty() {
                None
            } else {
                Some(ctrl_list)
            };
            // Only track/apply controls explicitly requested by the caller.
            let mut control_state: HashMap<ControlId, ControlValue> = HashMap::new();
            let mut readback_state: HashMap<ControlId, ControlValue> = HashMap::new();
            let mut controls_enabled = true;
            for (id, val) in &requested_controls_for_thread {
                control_state.insert(*id, val.clone());
            }
            if interval_for_thread.is_some() && supports_frame_duration_for_thread {
                if let Some(ae_id) = fps_controls_for_thread.ae_enable
                    && !requested_control_ids_for_thread.contains(&ae_id)
                {
                    control_state.insert(ae_id, ControlValue::Bool(false));
                }
                if let Some(exposure_id) = fps_controls_for_thread.exposure_time
                    && !requested_control_ids_for_thread.contains(&exposure_id)
                    && let Some(duration) = frame_duration
                {
                    let margin_us: i64 = 200;
                    let mut desired = (duration - margin_us).max(1);
                    if let Some(meta) = find_control_meta(&descriptor_for_thread, exposure_id) {
                        if let Some(min) = cv_i32(&meta.min) {
                            desired = desired.max(min as i64);
                        }
                        if let Some(max) = cv_i32(&meta.max) {
                            desired = desired.min(max as i64);
                        }
                    }
                    let desired_i32 = desired.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                    control_state.insert(exposure_id, ControlValue::Int(desired_i32));
                }
            }

            let req_rx = cam.subscribe_request_completed();
            let (ret_tx, ret_rx) = mpsc::channel::<Request>();
            if let Err(err) = cam.start(start_ctrls.as_ref().map(|c| &**c)) {
                let msg = err.to_string();
                if start_ctrls.is_some() && is_control_apply_error(&msg) {
                    controls_enabled = false;
                    start_ctrls = None;
                    control_state.clear();
                    frame_duration = None;
                    cam.start(None)
                        .map_err(|e| CaptureError::Backend(e.to_string()))?;
                } else {
                    return Err(CaptureError::Backend(msg));
                }
            }
            for req in requests {
                cam.queue_request(req)
                    .map_err(|(_, e)| CaptureError::Backend(e.to_string()))?;
            }

            let _ = setup_tx.send(Ok(validated_mode.clone()));

            let mut failure: Option<CaptureError> = None;
            loop {
                while let Ok(mut ret_req) = ret_rx.try_recv() {
                    ret_req.reuse(ReuseFlag::REUSE_BUFFERS);
                    let _ = queue_with_controls(&cam, ret_req, &control_state, frame_duration);
                }
                // Handle control messages.
                while let Ok(msg) = ctrl_rx.try_recv() {
                    match msg {
                        ControlMessage::Wake => {
                            if !controls_enabled {
                                let _ = pending_controls_for_thread
                                    .lock()
                                    .map(|mut guard| guard.updates.clear());
                                continue;
                            }
                            let updates = {
                                let mut guard = pending_controls_for_thread.lock().expect("libcamera pending lock poisoned");
                                std::mem::take(&mut guard.updates)
                            };
                            for (id, val) in updates {
                                if !writable_controls_for_thread.contains(&id) {
                                    continue;
                                }
                                match val {
                                    Some(val) => {
                                        if id == ControlId(30) {
                                            if let ControlValue::Int(v) = val {
                                                frame_duration = Some(v as i64);
                                            }
                                        } else {
                                            control_state.insert(id, val);
                                        }
                                    }
                                    None => {
                                        if id == ControlId(30) {
                                            frame_duration = None;
                                        } else {
                                            control_state.remove(&id);
                                        }
                                    }
                                }
                            }
                        }
                        ControlMessage::Get(id, resp_tx) => {
                            let pending = pending_controls_for_thread.lock().ok().and_then(|guard| guard.get(&id));
                            let resp = readback_state
                                .get(&id)
                                .cloned()
                                .or_else(|| pending.and_then(|val| val))
                                .or_else(|| control_state.get(&id).cloned())
                                .ok_or(CaptureError::ControlUnsupported);
                            let _ = resp_tx.send(resp);
                        }
                    }
                }

                match req_rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(req) => {
                        // Snapshot request metadata into a readback map.
                        //
                        // Do not overwrite `control_state` from request metadata.
                        //
                        // `control_state` represents the desired setpoints that we apply when
                        // re-queuing requests. Updating it from completed-request metadata can
                        // race with pending host updates and effectively make controls "stick"
                        // in one direction (e.g. increase works but decrease is immediately
                        // overwritten by the previous request's metadata).
                        //
                        //
                        // `readback_state` is best-effort and only tracks scalar control types that
                        // fit into Styx's ControlValue.
                        for (id, val) in req.metadata() {
                            let Some(val) = from_lc_value(&val) else { continue };
                            readback_state.insert(ControlId(id), val);
                        }

                        let (framebuffer, active_stride): (&MemoryMappedFrameBuffer<FrameBuffer>, usize) =
                            if let Some(tdn_stream) = &tdn_stream {
                                match req.buffer(tdn_stream) {
                                    Some(fb) => (fb, tdn_stride.unwrap_or(cfg_stride)),
                                    None => match req.buffer(&stream) {
                                        Some(fb) => (fb, cfg_stride),
                                        None => break,
                                    },
                                }
                            } else {
                                match req.buffer(&stream) {
                                    Some(fb) => (fb, cfg_stride),
                                    None => break,
                                }
                            };
                        let meta = match framebuffer.metadata() {
                            Some(m) => m,
                            None => break,
                        };
                        let timestamp = meta.timestamp();
                        let planes_meta = meta.planes();
                        let plane_slices = framebuffer.data();
                        let height = wire_format.resolution.height.get() as usize;
                        let mut layouts = smallvec::SmallVec::<[PlaneLayout; 3]>::new();
                        let mut plane_ptrs: Vec<(*const u8, usize)> = Vec::new();

                        // PiSP/libcamera streams often expose NV12/NV21 as a single contiguous plane
                        // (or a second empty plane). Split it into 2 logical planes so downstream
                        // NV12 decoders can operate.
                        let code = wire_format.code;
                        let is_nv12 =
                            code == FourCc::new(*b"NV12") || code == FourCc::new(*b"NV21");
                        if is_nv12 && !plane_slices.is_empty() {
                            let slice = plane_slices[0];
                            let slice_len = slice.len();
                            let total_len = planes_meta
                                .get(0)
                                .map(|m| m.bytes_used as usize)
                                .filter(|n| *n > 0)
                                .map(|n| n.min(slice_len))
                                .unwrap_or(slice_len);

                            let width = wire_format.resolution.width.get() as usize;
                            let y_height = height;
                            let uv_height = height / 2;
                            let denom = y_height.saturating_add(uv_height).max(1);

                            // Prefer libcamera-provided stride when present; otherwise infer stride
                            // from the total plane length for NV12 (Y + UV).
                            let inferred = total_len / denom;
                            let stride = if active_stride > 0 {
                                active_stride
                            } else {
                                inferred.max(width).max(1)
                            };

                            let y_len = stride.saturating_mul(y_height);
                            let uv_len = stride.saturating_mul(uv_height);
                            if y_len.saturating_add(uv_len) <= total_len && uv_height > 0 {
                                layouts.push(PlaneLayout {
                                    offset: 0,
                                    len: y_len,
                                    stride,
                                });
                                layouts.push(PlaneLayout {
                                    offset: y_len,
                                    len: uv_len,
                                    stride,
                                });
                                plane_ptrs.push((slice.as_ptr(), total_len));
                                plane_ptrs.push((slice.as_ptr(), total_len));
                            }
                        }

                        // Default: treat libcamera planes as-is.
                        if layouts.is_empty() {
                            layouts = planes_meta
                                .into_iter()
                                .enumerate()
                                .map(|(idx, plane_meta)| {
                                    let slice_len =
                                        plane_slices.get(idx).map(|s| s.len()).unwrap_or_default();
                                    let mut len = plane_meta.bytes_used as usize;
                                    if len == 0 {
                                        len = slice_len;
                                    } else {
                                        len = len.min(slice_len);
                                    }
                                    let plane_height = plane_height_for_format(code, idx, height);
                                    let stride = if idx == 0 && active_stride > 0 {
                                        // Some libcamera backends only report a single stride; keep it
                                        // for the first plane but clamp to the mapped slice.
                                        if plane_height == 0 {
                                            active_stride
                                        } else {
                                            let max_stride = slice_len / plane_height;
                                            active_stride.min(max_stride.max(1))
                                        }
                                    } else {
                                        infer_stride(len, slice_len, plane_height)
                                    };
                                    PlaneLayout {
                                        offset: 0,
                                        len,
                                        stride,
                                    }
                                })
                                .collect::<smallvec::SmallVec<[_; 3]>>();

                            for data in &plane_slices {
                                plane_ptrs.push((data.as_ptr(), data.len()));
                            }
                        }
                        let backing =
                            LibcameraBacking::new(req, ret_tx.clone(), plane_ptrs, shutting_down.clone());
                        let meta = FrameMeta::new(wire_format, timestamp);
                        let frame = FrameLease::from_external(meta, layouts, backing);
                        let frame = if let Some(emulation) = &emulation {
                            match emulation {
                                Emulation::Nv12ToRgb(dec) => match dec.process(frame) {
                                    Ok(out) => out,
                                    Err(e) => {
                                        failure = Some(CaptureError::Backend(format!(
                                            "nv12->rgb conversion failed: {e}"
                                        )));
                                        break;
                                    }
                                },
                                Emulation::Nv12ToBgr(dec) => match dec.process(frame) {
                                    Ok(out) => out,
                                    Err(e) => {
                                        failure = Some(CaptureError::Backend(format!(
                                            "nv12->bgr conversion failed: {e}"
                                        )));
                                        break;
                                    }
                                },
                                Emulation::YuyvToRgb(dec) => match dec.process(frame) {
                                    Ok(out) => out,
                                    Err(e) => {
                                        failure = Some(CaptureError::Backend(format!(
                                            "yuyv->rgb conversion failed: {e}"
                                        )));
                                        break;
                                    }
                                },
                            }
                        } else {
                            frame
                        };
                        if matches!(tx.send(frame), SendOutcome::Closed) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if stop_rx.try_recv().is_ok() {
                            shutting_down.store(true, Ordering::Release);
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            if let Some(err) = failure {
                Err(err)
            } else {
                Ok(validated_mode)
            }
        })();

        // After the capture worker exits, try to stop the shared CameraManager (best-effort).
        // This releases large PiSP/IPA allocations so idle memory stays low once all cameras
        // are closed.
        let _ = styx_libcamera::try_stop_if_idle();

        if let Err(e) = res {
            let _ = setup_tx.send(Err(e));
        }
    });

    let setup = setup_rx
        .recv()
        .unwrap_or_else(|_| Err(CaptureError::Backend("libcamera thread failed".into())));

    let mode = match setup {
        Ok(mode) => mode,
        Err(e) => return Err(e),
    };

    Ok(CaptureHandle {
        backend: BackendKind::Libcamera,
        control: ControlPlane::Libcamera { tx: ctrl_tx, pending: pending_controls },
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(WorkerHandle::Thread(worker)),
        metrics: StageMetrics::default(),
    })
}

fn build_libcamera_controls(
    controls: &[(ControlId, ControlValue)],
) -> Result<libcamera::utils::UniquePtr<LcControlList>, CaptureError> {
    let mut list = LcControlList::new();
    for (id, value) in controls {
        let v = to_lc_value(value)?;
        list.set_raw(id.0, v)
            .map_err(|e| CaptureError::ControlApply(e.to_string()))?;
    }
    Ok(list)
}

fn to_lc_value(value: &ControlValue) -> Result<LcValue, CaptureError> {
    let val = match value {
        ControlValue::None => LcValue::None,
        ControlValue::Bool(v) => LcValue::from(*v),
        ControlValue::Int(v) => LcValue::from(*v as i32),
        ControlValue::Uint(v) => LcValue::from(*v as u32),
        ControlValue::Float(v) => LcValue::from(*v),
    };
    Ok(val)
}

fn queue_with_controls(
    cam: &libcamera::camera::ActiveCamera<'_>,
    mut req: libcamera::request::Request,
    controls: &HashMap<ControlId, ControlValue>,
    frame_duration: Option<i64>,
) -> Result<(), ()> {
    {
        let list = req.controls_mut();
        for (id, val) in controls {
            if let Ok(lc_val) = to_lc_value(val) {
                let _ = list.set_raw(id.0, lc_val);
            }
        }
        if let Some(duration) = frame_duration {
            let _ = list.set_raw(30, LcValue::from([duration, duration]));
        }
    }
    cam.queue_request(req).map_err(|_| ())
}

struct LibcameraBacking {
    req: std::sync::Mutex<Option<libcamera::request::Request>>,
    planes: Vec<(*const u8, usize)>,
    ret_tx: std::sync::mpsc::Sender<libcamera::request::Request>,
    shutting_down: std::sync::Arc<AtomicBool>,
}

impl LibcameraBacking {
    fn new(
        req: libcamera::request::Request,
        ret_tx: std::sync::mpsc::Sender<libcamera::request::Request>,
        planes: Vec<(*const u8, usize)>,
        shutting_down: std::sync::Arc<AtomicBool>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            req: std::sync::Mutex::new(Some(req)),
            planes,
            ret_tx,
            shutting_down,
        })
    }
}

unsafe impl Send for LibcameraBacking {}
unsafe impl Sync for LibcameraBacking {}

impl ExternalBacking for LibcameraBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.planes
            .get(index)
            .map(|(ptr, len)| unsafe { std::slice::from_raw_parts(*ptr, *len) })
    }
}

impl Drop for LibcameraBacking {
    fn drop(&mut self) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        if let Some(req) = self.req.lock().unwrap().take() {
            let _ = self.ret_tx.send(req);
        }
    }
}

struct ShutdownGuard(std::sync::Arc<AtomicBool>);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Debug, Default)]
pub struct PendingControlState {
    updates: HashMap<ControlId, Option<ControlValue>>,
}

impl PendingControlState {
    fn get(&self, id: &ControlId) -> Option<Option<ControlValue>> {
        self.updates.get(id).cloned()
    }
}

impl std::ops::Deref for PendingControlState {
    type Target = HashMap<ControlId, Option<ControlValue>>;

    fn deref(&self) -> &Self::Target {
        &self.updates
    }
}

impl std::ops::DerefMut for PendingControlState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.updates
    }
}

pub enum ControlMessage {
    Wake,
    Get(
        ControlId,
        std::sync::mpsc::Sender<Result<ControlValue, CaptureError>>,
    ),
}
