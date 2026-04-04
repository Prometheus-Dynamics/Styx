use std::collections::{HashMap, HashSet};
#[cfg(feature = "v4l2")]
use std::fs;
#[cfg(feature = "v4l2")]
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use libcamera::framebuffer::AsFrameBuffer;
use libcamera::framebuffer_allocator::FrameBuffer;
use libcamera::request::Request;
use libcamera::request::ReuseFlag;
use libcamera::{control::ControlList as LcControlList, control_value::ControlValue as LcValue};
use smallvec::SmallVec;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use styx_codec::Codec;
use styx_codec::prelude::{Nv12ToBgrDecoder, Nv12ToRgbDecoder, YuyvToRgbDecoder};
use styx_core::prelude::*;

use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlApplyKind, ControlPlane, TdnOutputMode,
    WorkerHandle,
};
use crate::metrics::{ExternalBackingTracker, StageMetrics};
use crate::prelude::{Interval, Mode, ModeId};
use crate::{BackendHandle, BackendKind, ProbedBackend};

#[cfg(feature = "v4l2")]
const V4L2_CID_VBLANK: u32 = 0x009e0901;
const LIBCAMERA_FRAME_DURATION_LIMITS: ControlId = ControlId(30);

fn stop_when_idle_enabled() -> bool {
    std::env::var("STYX_LIBCAMERA_STOP_WHEN_IDLE")
        .ok()
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "" | "0" | "false" | "no" | "off")
        })
        .unwrap_or(false)
}

pub(super) fn stop_manager_if_idle() {
    if stop_when_idle_enabled() {
        let _ = styx_libcamera::try_stop_if_idle();
    }
}

fn prefault_request_pools_enabled() -> bool {
    std::env::var("STYX_LIBCAMERA_PREFAULT_REQUEST_POOLS")
        .ok()
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "0" | "false" | "no" | "off")
        })
        .unwrap_or(true)
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

fn processed_stream_role_override() -> Option<libcamera::stream::StreamRole> {
    let value = std::env::var("STYX_LIBCAMERA_PROCESSED_STREAM_ROLE")
        .ok()?
        .trim()
        .to_ascii_lowercase();
    match value.as_str() {
        "viewfinder" | "view-finder" | "vf" => Some(libcamera::stream::StreamRole::ViewFinder),
        "video" | "recording" | "video-recording" | "video_recording" => {
            Some(libcamera::stream::StreamRole::VideoRecording)
        }
        "still" | "still-capture" | "still_capture" => {
            Some(libcamera::stream::StreamRole::StillCapture)
        }
        _ => None,
    }
}

fn supports_frame_duration_limits(descriptor: &CaptureDescriptor) -> bool {
    descriptor
        .controls
        .iter()
        .any(|meta| meta.id == LIBCAMERA_FRAME_DURATION_LIMITS)
}

fn classify_libcamera_control_apply_kind(message: &str) -> ControlApplyKind {
    let msg = message.to_ascii_lowercase();
    if msg.contains("permission denied") {
        ControlApplyKind::PermissionDenied
    } else if msg.contains("invalid argument") {
        ControlApplyKind::InvalidArgument
    } else if msg.contains("set controls")
        || msg.contains("unable to set controls")
        || msg.contains("failed to set controls")
    {
        ControlApplyKind::SetControlsRejected
    } else {
        ControlApplyKind::Other
    }
}

fn classify_libcamera_control_apply_message(message: impl Into<String>) -> CaptureError {
    let message = message.into();
    let kind = classify_libcamera_control_apply_kind(&message);
    CaptureError::classified_control_apply(kind, message)
}

fn classify_libcamera_backend_message(message: impl Into<String>) -> CaptureError {
    let message = message.into();
    let msg = message.to_ascii_lowercase();
    if msg.contains("device or resource busy")
        || msg.contains("camera in running state")
        || msg.contains("resource busy")
    {
        CaptureError::LibcameraBusy(message)
    } else if msg.contains("tdn output not enabled") || msg.contains("tdn enabled") {
        CaptureError::LibcameraTdnConfigurationMismatch(message)
    } else {
        CaptureError::Backend(message)
    }
}

fn from_lc_value(value: &LcValue) -> Option<ControlValue> {
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
        _ => processed_stream_role_override().unwrap_or(libcamera::stream::StreamRole::ViewFinder),
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

fn wait_for_backings_to_drain(outstanding_backings: &AtomicUsize, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if outstanding_backings.load(Ordering::Acquire) == 0 {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn system_page_size() -> usize {
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 { ps as usize } else { 4096 }
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct BackingPlaneView {
    fd: i32,
    offset: usize,
    len: usize,
}

#[derive(Clone, Copy)]
struct MappedPlaneRange {
    ptr: *mut core::ffi::c_void,
    len: usize,
    map_offset: usize,
}

struct LazyMappedBackingState {
    mmaps: SmallVec<[(i32, MappedPlaneRange); 3]>,
    mapped_bytes: usize,
}

impl Drop for LazyMappedBackingState {
    fn drop(&mut self) {
        for (_fd, range) in self.mmaps.drain(..) {
            unsafe {
                libc::munmap(range.ptr, range.len);
            }
        }
    }
}

fn unique_backing_plane_bytes(planes: &[BackingPlaneView]) -> usize {
    let mut seen = SmallVec::<[(i32, usize, usize); 4]>::new();
    planes
        .iter()
        .filter(|plane| {
            let key = (plane.fd, plane.offset, plane.len);
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        })
        .map(|plane| plane.len)
        .sum()
}

fn framebuffer_backing_planes(buffer: &FrameBuffer) -> SmallVec<[BackingPlaneView; 3]> {
    let planes = buffer.planes();
    let mut views = SmallVec::<[BackingPlaneView; 3]>::with_capacity(planes.len());
    for idx in 0..planes.len() {
        let Some(plane) = planes.get(idx) else {
            break;
        };
        views.push(BackingPlaneView {
            fd: plane.fd(),
            offset: plane.offset().unwrap_or(0),
            len: plane.len(),
        });
    }
    views
}

fn framebuffers_backing_planes(buffers: &[FrameBuffer]) -> SmallVec<[BackingPlaneView; 12]> {
    let mut views = SmallVec::<[BackingPlaneView; 12]>::new();
    for buffer in buffers {
        views.extend(framebuffer_backing_planes(buffer));
    }
    views
}

fn map_backing_planes(planes: &[BackingPlaneView]) -> Option<LazyMappedBackingState> {
    struct MapInfo {
        start: usize,
        end: usize,
        total_len: usize,
    }

    let page_size = system_page_size();

    let mut map_info = SmallVec::<[(i32, MapInfo); 3]>::new();
    for plane in planes {
        let end = plane.offset.checked_add(plane.len)?;
        let info = if let Some((_, info)) = map_info.iter_mut().find(|(fd, _)| *fd == plane.fd) {
            info
        } else {
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            let ret = unsafe { libc::fstat(plane.fd, st.as_mut_ptr()) };
            let total_len = if ret != 0 {
                0
            } else {
                let st = unsafe { st.assume_init() };
                st.st_size as usize
            };
            map_info.push((
                plane.fd,
                MapInfo {
                    start: plane.offset,
                    end,
                    total_len,
                },
            ));
            &mut map_info
                .last_mut()
                .expect("backing map info entry just pushed")
                .1
        };

        if info.total_len > 0 && end > info.total_len {
            return None;
        }

        let aligned_start = plane.offset - (plane.offset % page_size);
        info.start = info.start.min(aligned_start);
        info.end = info.end.max(end);
    }

    let mut mapped_bytes = 0usize;
    let mut mmaps = SmallVec::<[(i32, MappedPlaneRange); 3]>::new();
    for (fd, info) in map_info {
        let map_len = info.end.saturating_sub(info.start);
        if map_len == 0 {
            continue;
        }
        let addr = unsafe {
            libc::mmap64(
                core::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                info.start as _,
            )
        };
        if addr == libc::MAP_FAILED {
            return None;
        }
        mapped_bytes = mapped_bytes.saturating_add(map_len);
        mmaps.push((
            fd,
            MappedPlaneRange {
                ptr: addr,
                len: map_len,
                map_offset: info.start,
            },
        ));
    }

    Some(LazyMappedBackingState {
        mmaps,
        mapped_bytes,
    })
}

fn prefault_backing_planes(planes: &[BackingPlaneView]) {
    let Some(mapped) = map_backing_planes(planes) else {
        return;
    };
    let page_size = system_page_size();
    let mut touched = 0u8;
    for (_, range) in mapped.mmaps.iter() {
        let ptr = range.ptr.cast::<u8>();
        let mut offset = 0usize;
        while offset < range.len {
            unsafe {
                touched ^= std::ptr::read_volatile(ptr.add(offset));
            }
            offset = offset.saturating_add(page_size);
        }
        if range.len > 0 {
            unsafe {
                touched ^= std::ptr::read_volatile(ptr.add(range.len - 1));
            }
        }
    }
    std::hint::black_box(touched);
}

struct RequestPoolBackingLease {
    tracker: Arc<ExternalBackingTracker>,
    buffers: usize,
    bytes: usize,
}

impl RequestPoolBackingLease {
    fn new(tracker: Arc<ExternalBackingTracker>, framebuffers: &[FrameBuffer]) -> Self {
        let buffers = framebuffers.len();
        let planes = framebuffers_backing_planes(framebuffers);
        let bytes = unique_backing_plane_bytes(&planes);
        tracker.acquire_many(buffers, bytes);
        if prefault_request_pools_enabled() && !planes.is_empty() {
            // Libcamera request pools are persistent across the whole capture session. Touch them
            // once up front so the working set does not trickle in over the first minutes of
            // preview and look like a leak.
            prefault_backing_planes(&planes);
        }
        Self {
            tracker,
            buffers,
            bytes,
        }
    }
}

impl Drop for RequestPoolBackingLease {
    fn drop(&mut self) {
        self.tracker.release_many(self.buffers, self.bytes);
    }
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

    let supports_frame_duration = supports_frame_duration_limits(&descriptor);

    let enable_tdn_output_for_thread = enable_tdn_output;
    let id_for_thread = id.clone();
    let writable_controls_for_thread = writable_controls.clone();
    let requested_controls_for_thread = requested_controls.clone();
    let interval_for_thread = interval;
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
    let pending_controls =
        std::sync::Arc::new(std::sync::Mutex::new(PendingControlState::default()));
    let outstanding_backings = Arc::new(AtomicUsize::new(0));
    let lease_backing_tracker = Arc::new(ExternalBackingTracker::new("libcamera_dmabuf_lease"));
    let request_pool_tracker =
        Arc::new(ExternalBackingTracker::new("libcamera_dmabuf_request_pool"));
    let tdn_request_pool_tracker = Arc::new(ExternalBackingTracker::new(
        "libcamera_dmabuf_tdn_request_pool",
    ));
    let mode_for_thread = mode.clone();

    let pending_controls_for_thread = pending_controls.clone();
    let outstanding_backings_for_thread = outstanding_backings.clone();
    let lease_backing_tracker_for_thread = lease_backing_tracker.clone();
    let request_pool_tracker_for_thread = request_pool_tracker.clone();
    let tdn_request_pool_tracker_for_thread = tdn_request_pool_tracker.clone();
    let worker = thread::spawn(move || {
        let res: Result<Mode, CaptureError> = (|| {
            let shutting_down = std::sync::Arc::new(AtomicBool::new(false));
            let _shutdown_guard = ShutdownGuard(shutting_down.clone());
            let mgr = styx_libcamera::manager().map_err(classify_libcamera_backend_message)?;
            let camera_lookup_started = Instant::now();
            let mut cam = loop {
                let cameras = mgr.cameras();
                let seen_camera_ids = (0..cameras.len())
                    .filter_map(|idx| cameras.get(idx).map(|cam| cam.id().to_string()))
                    .collect();
                let cam = (0..cameras.len()).find_map(|idx| {
                    let cam = cameras.get(idx)?;
                    if cam.id() == id_for_thread {
                        Some(cam)
                    } else {
                        None
                    }
                });
                if let Some(cam) = cam {
                    break cam;
                }
                if camera_lookup_started.elapsed() >= Duration::from_secs(3) {
                    return Err(CaptureError::LibcameraCameraNotFound {
                        requested: id_for_thread.clone(),
                        seen: seen_camera_ids,
                    });
                }
                thread::sleep(Duration::from_millis(100));
            }
            .acquire()
            .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;

            let role = stream_role_for_request(mode_for_thread.format.code);
            let enable_tdn_output = enable_tdn_output_for_thread;
            let mut roles = vec![role];
            if enable_tdn_output {
                roles.push(libcamera::stream::StreamRole::VideoRecording);
            }
            let mut cfgs = cam
                .generate_configuration(&roles)
                .ok_or(CaptureError::LibcameraGenerateConfigurationFailed)?;
            if enable_tdn_output && cfgs.get(1).is_none() {
                return Err(CaptureError::LibcameraTdnOutputUnavailable);
            }
            let requested_code = mode_for_thread.format.code;
            if is_rpi_pisp_sensor_i2c(&id_for_thread) && pisp_disallowed_fourcc(requested_code) {
                return Err(CaptureError::Backend(format!(
                    "{} unsupported on PiSP",
                    requested_code
                )));
            }
            let libcamera_code = normalize_requested_fourcc_for_libcamera(requested_code);
            let is_rgb24_request =
                matches!(&libcamera_code.to_u32().to_le_bytes(), b"RGB3" | b"BGR3");
            let emulate_rgb24 = is_rgb24_request && is_rpi_pisp_sensor_i2c(&id_for_thread);

            // PiSP (rpi/pisp) currently asserts/crashes in libcamera when validating sensor-camera
            // configs that request RGB24/BGR24. To keep the API true to the requested format, we
            // capture YUV (NV12 preferred) and convert to the requested RGB/BGR in software.
            {
                // Default queue depth is still 4, but low-memory profiles can deliberately run
                // with a single in-flight buffer to reduce libcamera/TDN pool residency.
                let depth_u32 = u32::try_from(queue_depth).unwrap_or(4).clamp(1, 12);
                let mut cfg = cfgs
                    .get_mut(0)
                    .ok_or_else(|| CaptureError::Backend("missing stream config".into()))?;
                let desired_format = if emulate_rgb24 {
                    FourCc::new(*b"NV12")
                } else {
                    libcamera_code
                };
                cfg.set_pixel_format(libcamera::pixel_format::PixelFormat::new(
                    desired_format.to_u32(),
                    0,
                ));
                cfg.set_size(Size::new(
                    mode_for_thread.format.resolution.width.get(),
                    mode_for_thread.format.resolution.height.get(),
                ));
                cfg.set_buffer_count(depth_u32);

                if enable_tdn_output
                    && let Some(mut tdn_cfg) = cfgs.get_mut(1)
                {
                    tdn_cfg.set_pixel_format(libcamera::pixel_format::PixelFormat::new(
                        desired_format.to_u32(),
                        0,
                    ));
                    tdn_cfg.set_size(Size::new(
                        mode_for_thread.format.resolution.width.get(),
                        mode_for_thread.format.resolution.height.get(),
                    ));
                    tdn_cfg.set_buffer_count(depth_u32);
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
                    if enable_tdn_output
                        && let Some(mut tdn_cfg) = cfgs.get_mut(1)
                    {
                        tdn_cfg.set_pixel_format(libcamera::pixel_format::PixelFormat::new(
                            FourCc::new(*b"YUYV").to_u32(),
                            0,
                        ));
                    }
                    if matches!(cfgs.validate(), CameraConfigurationStatus::Invalid) {
                        return Err(CaptureError::Backend("config invalid".into()));
                    }
                } else {
                    return Err(CaptureError::Backend("config invalid".into()));
                }
            }
            cam.configure(&mut cfgs)
                .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;

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
            let wire_format =
                MediaFormat::new(validated_code, validated_res, mode_for_thread.format.color);
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
                .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;
            let tdn_bufs = if let Some(tdn_stream) = &tdn_stream {
                Some(
                    alloc
                        .alloc(tdn_stream)
                        .map_err(|e| classify_libcamera_backend_message(e.to_string()))?,
                )
            } else {
                None
            };
            let primary_buffers: Vec<FrameBuffer> = bufs.into_iter().collect();
            let tdn_buffers: Option<Vec<FrameBuffer>> =
                tdn_bufs.map(|bufs| bufs.into_iter().collect());
            let _primary_request_pool_lease =
                RequestPoolBackingLease::new(request_pool_tracker_for_thread, &primary_buffers);
            let _tdn_request_pool_lease = tdn_buffers.as_ref().map(|buffers| {
                RequestPoolBackingLease::new(tdn_request_pool_tracker_for_thread, buffers)
            });

            let mut requests = Vec::new();
            if let Some(tdn_stream) = &tdn_stream {
                let Some(tdn_buffers) = tdn_buffers else {
                    return Err(CaptureError::LibcameraTdnOutputUnavailable);
                };
                if tdn_buffers.is_empty() {
                    return Err(CaptureError::LibcameraTdnOutputUnavailable);
                }
                for ((i, buf), tdn_buf) in primary_buffers
                    .into_iter()
                    .enumerate()
                    .zip(tdn_buffers.into_iter())
                {
                    let mut req = cam
                        .create_request(Some(i as u64))
                        .ok_or_else(|| CaptureError::Backend("request create failed".into()))?;
                    req.add_buffer(&stream, buf)
                        .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;
                    req.add_buffer(tdn_stream, tdn_buf)
                        .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;
                    requests.push(req);
                }
            } else {
                for (i, buf) in primary_buffers.into_iter().enumerate() {
                    let mut req = cam
                        .create_request(Some(i as u64))
                        .ok_or_else(|| CaptureError::Backend("request create failed".into()))?;
                    req.add_buffer(&stream, buf)
                        .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;
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
                ctrl_list
                    .set_raw(30, LcValue::from([duration, duration]))
                    .map_err(|e| classify_libcamera_control_apply_message(e.to_string()))?;
            }
            let start_ctrls = if ctrl_list.is_empty() {
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
            let req_rx = cam.subscribe_request_completed();
            let (ret_tx, ret_rx) = mpsc::channel::<Request>();
            if let Err(err) = cam.start(start_ctrls.as_deref()) {
                let msg = err.to_string();
                if start_ctrls.is_some()
                    && classify_libcamera_control_apply_kind(&msg) != ControlApplyKind::Other
                {
                    controls_enabled = false;
                    control_state.clear();
                    frame_duration = None;
                    cam.start(None)
                        .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;
                } else {
                    return Err(classify_libcamera_backend_message(msg));
                }
            }
            for req in requests {
                cam.queue_request(req)
                    .map_err(|(_, e)| classify_libcamera_backend_message(e.to_string()))?;
            }

            let _ = setup_tx.send(Ok(validated_mode.clone()));

            let mut failure: Option<CaptureError> = None;
            let mut pending_requeue: Vec<Request> = Vec::new();
            let mut requeue_fail_since: Option<Instant> = None;
            loop {
                while let Ok(mut ret_req) = ret_rx.try_recv() {
                    ret_req.reuse(ReuseFlag::REUSE_BUFFERS);
                    pending_requeue.push(ret_req);
                }
                if !pending_requeue.is_empty() {
                    let mut still_pending = Vec::with_capacity(pending_requeue.len());
                    for ret_req in pending_requeue.drain(..) {
                        match queue_with_controls(&cam, ret_req, &control_state, frame_duration) {
                            Ok(()) => {}
                            Err(ret_req) => still_pending.push(ret_req),
                        }
                    }
                    if still_pending.is_empty() {
                        requeue_fail_since = None;
                    } else {
                        if requeue_fail_since.is_none() {
                            requeue_fail_since = Some(Instant::now());
                        }
                        if requeue_fail_since
                            .is_some_and(|since| since.elapsed() >= Duration::from_secs(2))
                        {
                            failure = Some(CaptureError::Backend(format!(
                                "libcamera request requeue stalled for {} buffers",
                                still_pending.len()
                            )));
                            break;
                        }
                    }
                    pending_requeue = still_pending;
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
                                let mut guard = pending_controls_for_thread
                                    .lock()
                                    .expect("libcamera pending lock poisoned");
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
                            let pending = pending_controls_for_thread
                                .lock()
                                .ok()
                                .and_then(|guard| guard.get(&id));
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
                            let Some(val) = from_lc_value(&val) else {
                                continue;
                            };
                            readback_state.insert(ControlId(id), val);
                        }

                        let (framebuffer, active_stride): (&FrameBuffer, usize) =
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
                        let (timestamp, layouts, plane_views) = {
                            let meta = match framebuffer.metadata() {
                                Some(m) => m,
                                None => break,
                            };
                            let timestamp = meta.timestamp();
                            let planes_meta = meta.planes();
                            let framebuffer_planes = framebuffer.planes();
                            let height = wire_format.resolution.height.get() as usize;
                            let mut layouts = smallvec::SmallVec::<[PlaneLayout; 3]>::new();
                            let mut plane_views = SmallVec::<[BackingPlaneView; 3]>::new();

                            // PiSP/libcamera streams often expose NV12/NV21 as a single contiguous plane
                            // (or a second empty plane). Split it into 2 logical planes so downstream
                            // NV12 decoders can operate.
                            let code = wire_format.code;
                            let is_nv12 =
                                code == FourCc::new(*b"NV12") || code == FourCc::new(*b"NV21");
                            if is_nv12 && !framebuffer_planes.is_empty() {
                                let first_plane = framebuffer_planes.get(0);
                                let Some(first_plane) = first_plane else {
                                    break;
                                };
                                let Some(first_offset) = first_plane.offset() else {
                                    break;
                                };
                                let slice_len = first_plane.len();
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
                                    plane_views.push(BackingPlaneView {
                                        fd: first_plane.fd(),
                                        offset: first_offset,
                                        len: total_len,
                                    });
                                    plane_views.push(BackingPlaneView {
                                        fd: first_plane.fd(),
                                        offset: first_offset,
                                        len: total_len,
                                    });
                                }
                            }

                            // Default: treat libcamera planes as-is.
                            if layouts.is_empty() {
                                layouts = planes_meta
                                    .into_iter()
                                    .enumerate()
                                    .map(|(idx, plane_meta)| {
                                        let slice_len = framebuffer_planes
                                            .get(idx)
                                            .map(|plane| plane.len())
                                            .unwrap_or_default();
                                        let mut len = plane_meta.bytes_used as usize;
                                        if len == 0 {
                                            len = slice_len;
                                        } else {
                                            len = len.min(slice_len);
                                        }
                                        let plane_height =
                                            plane_height_for_format(code, idx, height);
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

                                for idx in 0..framebuffer_planes.len() {
                                    let Some(plane) = framebuffer_planes.get(idx) else {
                                        break;
                                    };
                                    let Some(offset) = plane.offset() else {
                                        break;
                                    };
                                    plane_views.push(BackingPlaneView {
                                        fd: plane.fd(),
                                        offset,
                                        len: plane.len(),
                                    });
                                }
                            }
                            (timestamp, layouts, plane_views)
                        };
                        if plane_views.len() != layouts.len() {
                            failure = Some(CaptureError::Backend(
                                "libcamera plane layout mismatch".into(),
                            ));
                            break;
                        }
                        let backing = LibcameraBacking::new(
                            req,
                            ret_tx.clone(),
                            plane_views,
                            shutting_down.clone(),
                            outstanding_backings_for_thread.clone(),
                            lease_backing_tracker_for_thread.clone(),
                        );
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

        // Give any frames still being unwound by downstream consumers a short chance to release
        // their libcamera request/framebuffer backing before we attempt to stop the shared
        // CameraManager. Stopping too early can race with request/framebuffer destruction.
        //
        // PiSP dual-stream TDN sessions are currently unsafe to finalise by tearing the shared
        // CameraManager down immediately on worker exit. In practice the old TDN-enabled backend
        // can throw `BackEnd::finalise: TDN output not enabled when TDN enabled` during
        // finalisation if we stop the manager as part of an on->off restart. Leave the shared
        // manager running in that case; a later non-TDN idle stop can still reclaim it.
        if stop_when_idle_enabled()
            && !enable_tdn_output_for_thread
            && wait_for_backings_to_drain(&outstanding_backings_for_thread, Duration::from_secs(2))
        {
            let _ = styx_libcamera::try_stop_if_idle();
        }

        if let Err(e) = res {
            let _ = setup_tx.send(Err(e));
        }
    });

    let setup = setup_rx.recv().unwrap_or_else(|_| {
        Err(classify_libcamera_backend_message(
            "libcamera thread failed",
        ))
    });

    let mode = setup?;

    Ok(CaptureHandle {
        backend: BackendKind::Libcamera,
        control: ControlPlane::Libcamera {
            tx: ctrl_tx,
            pending: pending_controls,
        },
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(WorkerHandle::Thread(worker)),
        libcamera_idle_stop_allowed: !enable_tdn_output_for_thread,
        metrics: StageMetrics::default(),
        external_backings: vec![
            lease_backing_tracker,
            request_pool_tracker,
            tdn_request_pool_tracker,
        ],
    })
}

fn build_libcamera_controls(
    controls: &[(ControlId, ControlValue)],
) -> Result<libcamera::utils::UniquePtr<LcControlList>, CaptureError> {
    let mut list = LcControlList::new();
    for (id, value) in controls {
        let v = to_lc_value(value)?;
        list.set_raw(id.0, v)
            .map_err(|e| classify_libcamera_control_apply_message(e.to_string()))?;
    }
    Ok(list)
}

fn to_lc_value(value: &ControlValue) -> Result<LcValue, CaptureError> {
    let val = match value {
        ControlValue::None => LcValue::None,
        ControlValue::Bool(v) => LcValue::from(*v),
        ControlValue::Int(v) => LcValue::from(*v),
        ControlValue::Uint(v) => LcValue::from(*v),
        ControlValue::Float(v) => LcValue::from(*v),
    };
    Ok(val)
}

fn queue_with_controls(
    cam: &libcamera::camera::ActiveCamera<'_>,
    mut req: libcamera::request::Request,
    controls: &HashMap<ControlId, ControlValue>,
    frame_duration: Option<i64>,
) -> Result<(), libcamera::request::Request> {
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
    cam.queue_request(req).map_err(|(req, _)| req)
}

struct LibcameraBacking {
    req: std::sync::Mutex<Option<libcamera::request::Request>>,
    planes: SmallVec<[BackingPlaneView; 3]>,
    mapped: OnceLock<Option<LazyMappedBackingState>>,
    ret_tx: std::sync::mpsc::Sender<libcamera::request::Request>,
    shutting_down: std::sync::Arc<AtomicBool>,
    outstanding_backings: Arc<AtomicUsize>,
    tracker: Arc<ExternalBackingTracker>,
    backing_bytes: usize,
}

impl LibcameraBacking {
    fn new(
        req: libcamera::request::Request,
        ret_tx: std::sync::mpsc::Sender<libcamera::request::Request>,
        planes: SmallVec<[BackingPlaneView; 3]>,
        shutting_down: std::sync::Arc<AtomicBool>,
        outstanding_backings: Arc<AtomicUsize>,
        tracker: Arc<ExternalBackingTracker>,
    ) -> std::sync::Arc<Self> {
        let backing_bytes = unique_backing_plane_bytes(&planes);
        outstanding_backings.fetch_add(1, Ordering::AcqRel);
        std::sync::Arc::new(Self {
            req: std::sync::Mutex::new(Some(req)),
            planes,
            mapped: OnceLock::new(),
            ret_tx,
            shutting_down,
            outstanding_backings,
            tracker,
            backing_bytes,
        })
    }

    fn mapped_state(&self) -> Option<&LazyMappedBackingState> {
        self.mapped
            .get_or_init(|| {
                let mapped = map_backing_planes(&self.planes);
                if let Some(state) = mapped.as_ref() {
                    self.tracker.acquire(state.mapped_bytes);
                }
                mapped
            })
            .as_ref()
    }
}

unsafe impl Send for LibcameraBacking {}
unsafe impl Sync for LibcameraBacking {}

impl ExternalBacking for LibcameraBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let plane = self.planes.get(index)?;
        let mapped = self.mapped_state()?;
        let (_, range) = mapped.mmaps.iter().find(|(fd, _)| *fd == plane.fd)?;
        let offset = plane.offset.checked_sub(range.map_offset)?;
        let ptr: *const u8 = range.ptr.cast();
        Some(unsafe { std::slice::from_raw_parts(ptr.add(offset), plane.len) })
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.backing_bytes)
    }

    fn backing_kind(&self) -> &'static str {
        "libcamera_dmabuf"
    }
}

impl Drop for LibcameraBacking {
    fn drop(&mut self) {
        if let Some(mapped) = self.mapped.take().flatten() {
            self.tracker.release(mapped.mapped_bytes);
            drop(mapped);
        }
        if self.shutting_down.load(Ordering::Acquire) {
            self.outstanding_backings.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        if let Some(req) = self.req.lock().unwrap().take() {
            let _ = self.ret_tx.send(req);
        }
        self.outstanding_backings.fetch_sub(1, Ordering::AcqRel);
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
