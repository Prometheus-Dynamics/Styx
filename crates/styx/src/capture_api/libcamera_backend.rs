mod backing;
mod controls;
mod util;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use libcamera::framebuffer_allocator::FrameBuffer;
use libcamera::request::ReuseFlag;
use smallvec::SmallVec;
use styx_codec::Codec;
use styx_codec::prelude::{Nv12ToBgrDecoder, Nv12ToRgbDecoder, YuyvToRgbDecoder};
use styx_core::controls::ControlValue;
use styx_core::prelude::*;

use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlApplyKind, ControlPlane, TdnOutputMode,
    WorkerHandle,
};
use crate::metrics::{ExternalBackingTracker, StageMetrics};
use crate::prelude::{Interval, Mode, ModeId};
use crate::{BackendHandle, BackendKind, ProbedBackend};

use self::backing::{
    BackingPlaneView, LibcameraBacking, RequestPoolBackingLease, ShutdownGuard,
    wait_for_backings_to_drain,
};
pub use self::controls::{ControlMessage, PendingControlState};
use self::controls::{build_libcamera_controls, queue_with_controls};
use self::util::{
    classify_libcamera_backend_message, classify_libcamera_control_apply_kind,
    control_value_enabled, from_lc_value, map_pixel_format_to_fourcc,
    normalize_requested_fourcc_for_libcamera, pisp_disallowed_fourcc, plane_height_for_format,
    processed_stream_role_override, stream_role_for_request, supports_frame_duration_limits,
    to_lc_value,
};

pub(super) fn stop_manager_if_idle() {
    if util::stop_when_idle_enabled() {
        let _ = styx_libcamera::try_stop_if_idle();
    }
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
    if enable_tdn_output && !util::is_rpi_pisp_sensor_i2c(&id) {
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
            if util::is_rpi_pisp_sensor_i2c(&id_for_thread)
                && pisp_disallowed_fourcc(requested_code)
            {
                return Err(CaptureError::Backend(format!(
                    "{} unsupported on PiSP",
                    requested_code
                )));
            }
            let libcamera_code = normalize_requested_fourcc_for_libcamera(requested_code);
            let is_rgb24_request =
                matches!(&libcamera_code.to_u32().to_le_bytes(), b"RGB3" | b"BGR3");
            let emulate_rgb24 = is_rgb24_request && util::is_rpi_pisp_sensor_i2c(&id_for_thread);

            {
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

                if enable_tdn_output && let Some(mut tdn_cfg) = cfgs.get_mut(1) {
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
                    if enable_tdn_output && let Some(mut tdn_cfg) = cfgs.get_mut(1) {
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
                    util::try_set_sensor_vblank_min_for_high_fps(&id_for_thread);
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
                let num = interval.numerator.get() as u64;
                let den = interval.denominator.get() as u64;
                let duration_us = num.saturating_mul(1_000_000).saturating_div(den.max(1));
                let duration = duration_us.clamp(1, i64::MAX as u64) as i64;
                frame_duration = Some(duration);
                ctrl_list
                    .set_raw(
                        30,
                        libcamera::control_value::ControlValue::from([duration, duration]),
                    )
                    .map_err(|e| util::classify_libcamera_control_apply_message(e.to_string()))?;
            }
            let start_ctrls = if ctrl_list.is_empty() {
                None
            } else {
                Some(ctrl_list)
            };
            let mut control_state: HashMap<ControlId, ControlValue> = HashMap::new();
            let mut readback_state: HashMap<ControlId, ControlValue> = HashMap::new();
            let mut controls_enabled = true;
            for (id, val) in &requested_controls_for_thread {
                control_state.insert(*id, val.clone());
            }
            let req_rx = cam.subscribe_request_completed();
            let (ret_tx, ret_rx) = mpsc::channel::<libcamera::request::Request>();
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
            let mut pending_requeue: Vec<libcamera::request::Request> = Vec::new();
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
                                            if plane_height == 0 {
                                                active_stride
                                            } else {
                                                let max_stride = slice_len / plane_height;
                                                active_stride.min(max_stride.max(1))
                                            }
                                        } else {
                                            backing::infer_stride(len, slice_len, plane_height)
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
                        let meta = FrameMeta::new(wire_format, timestamp)
                            .with_capture_instant(std::time::Instant::now())
                            .with_transition(ResidencyTransition {
                                from: FrameResidency::Dmabuf,
                                to: FrameResidency::Dmabuf,
                                reason: ResidencyTransitionReason::Capture,
                                copied: false,
                            });
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

        if util::stop_when_idle_enabled()
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
