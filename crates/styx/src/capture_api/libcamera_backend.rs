mod backing;
mod controls;
mod emulation;
mod frame;
mod heap;
mod streams;
mod util;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use libcamera::framebuffer::AsFrameBuffer;
use libcamera::request::ReuseFlag;
use parking_lot::Mutex;
use styx_core::controls::ControlValue;
use styx_core::prelude::*;

use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlApplyKind, ControlPlane,
    LIBCAMERA_FRAME_DURATION_LIMITS, StyxConfig, TdnOutputMode, WorkerHandle,
};
use crate::metrics::{ExternalBackingTracker, StageMetrics};
use crate::prelude::{Interval, Mode, ModeId};
use crate::{BackendHandle, BackendKind, ProbedBackend};

use self::backing::{
    LibcameraBacking, RequestPoolBackingLease, ShutdownGuard, wait_for_backings_to_drain,
};
pub use self::controls::{ControlMessage, PendingControlState};
use self::controls::{build_libcamera_controls, queue_with_controls, read_request_metadata};
use self::emulation::Emulation;
use self::frame::completed_frame_parts;
use self::heap::{CaptureBuffer, request_buffer};
use self::streams::{SecondStream, attach_companion, configure_streams, framebuffer_refs};
use self::util::{
    classify_libcamera_backend_message, classify_libcamera_control_apply_kind,
    control_value_enabled, map_pixel_format_to_fourcc, normalize_requested_fourcc_for_libcamera,
    pisp_disallowed_fourcc, stream_role_for_request, supports_frame_duration_limits,
};
use super::handle::{CaptureQueue, enqueue_capture_frame, record_worker_error};

pub(super) fn stop_manager_if_idle(configured: bool) {
    if util::stop_when_idle_enabled(configured) {
        let _ = styx_libcamera::try_stop_if_idle();
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start_libcamera(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
    tdn_output_mode: TdnOutputMode,
    config: &StyxConfig,
    queue: Option<CaptureQueue>,
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
    let capture_tunables = config.capture_tunables();
    let timestamp_clock = capture_tunables.timestamp_clock;
    let extra_buffers = capture_tunables.extra_buffers;
    let sequence_gaps = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut sequence_tracker = crate::metrics::SequenceGapTracker::new(sequence_gaps.clone());
    let libcamera_config = config.libcamera_config();
    let queue_depth = capture_tunables.queue_depth;
    let _ = requested_fps;
    let (tx, rx) = queue.unwrap_or_else(|| {
        styx_core::queue::bounded_with(queue_depth, capture_tunables.queue_overflow)
    });
    let (setup_tx, setup_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let (ctrl_tx, ctrl_rx) = mpsc::channel();
    let pending_controls = Arc::new(Mutex::new(PendingControlState::default()));
    let worker_error = Arc::new(Mutex::new(None));
    let outstanding_backings = Arc::new(AtomicUsize::new(0));
    let outstanding_lease_tracker = Arc::new(ExternalBackingTracker::new(
        "libcamera_dmabuf_outstanding_lease",
    ));
    let mapped_lease_tracker =
        Arc::new(ExternalBackingTracker::new("libcamera_dmabuf_mapped_lease"));
    let request_pool_tracker =
        Arc::new(ExternalBackingTracker::new("libcamera_dmabuf_request_pool"));
    let tdn_request_pool_tracker = Arc::new(ExternalBackingTracker::new(
        "libcamera_dmabuf_tdn_request_pool",
    ));
    let mode_for_thread = mode.clone();

    let pending_controls_for_thread = pending_controls.clone();
    let outstanding_backings_for_thread = outstanding_backings.clone();
    let outstanding_lease_tracker_for_thread = outstanding_lease_tracker.clone();
    let mapped_lease_tracker_for_thread = mapped_lease_tracker.clone();
    let request_pool_tracker_for_thread = request_pool_tracker.clone();
    let tdn_request_pool_tracker_for_thread = tdn_request_pool_tracker.clone();
    let worker_error_for_thread = worker_error.clone();
    let worker = thread::spawn(move || {
        let lookup_timeout = Duration::from_millis(libcamera_config.lookup_timeout_ms);
        let lookup_poll = Duration::from_millis(libcamera_config.lookup_poll_ms);
        let requeue_stall_timeout =
            Duration::from_millis(libcamera_config.requeue_stall_timeout_ms);
        let request_poll = Duration::from_millis(libcamera_config.request_poll_ms);
        let queue_send_timeout = Duration::from_millis(capture_tunables.queue_send_timeout_ms);
        let idle_drain_timeout = Duration::from_millis(libcamera_config.idle_drain_timeout_ms);
        let idle_drain_poll = Duration::from_millis(libcamera_config.idle_drain_poll_ms);
        let res: Result<Mode, CaptureError> = (|| {
            let camera_use =
                styx_libcamera::begin_camera_use().map_err(classify_libcamera_backend_message)?;
            let shutting_down = std::sync::Arc::new(AtomicBool::new(false));
            let _shutdown_guard = ShutdownGuard(shutting_down.clone());
            let camera_lookup_started = Instant::now();
            let mut cam = loop {
                let (cam, seen_camera_ids) =
                    styx_libcamera::find_camera(&camera_use, &id_for_thread)
                        .map_err(classify_libcamera_backend_message)?;
                if let Some(cam) = cam {
                    break cam;
                }
                if camera_lookup_started.elapsed() >= lookup_timeout {
                    return Err(CaptureError::LibcameraCameraNotFound {
                        requested: id_for_thread.clone(),
                        seen: seen_camera_ids,
                    });
                }
                thread::sleep(lookup_poll);
            }
            .acquire()
            .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;

            let role = stream_role_for_request(
                mode_for_thread.format.code,
                libcamera_config.processed_stream_role,
            );
            let enable_tdn_output = enable_tdn_output_for_thread;
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
            // Buffers for the queue plus headroom, so a full queue never leaves libcamera
            // without requests (it would then hand out its oldest raw frames).
            let depth_u32 = u32::try_from(queue_depth + extra_buffers)
                .unwrap_or(4)
                .clamp(1, 16);
            // The ISP scales to a requested output size; otherwise the mode's size.
            let (width, height) = libcamera_config.output_size.unwrap_or((
                mode_for_thread.format.resolution.width.get(),
                mode_for_thread.format.resolution.height.get(),
            ));
            let size = Size::new(width, height);
            // Pyramid companions need a processed format whose Y plane is directly usable.
            let pyramid_level = libcamera_config.pyramid_level.min(3);
            let mut second = if enable_tdn_output {
                SecondStream::Tdn
            } else if pyramid_level > 0 && !emulate_rgb24 {
                SecondStream::Pyramid(pyramid_level)
            } else {
                SecondStream::None
            };
            if pyramid_level > 0 && second != SecondStream::Pyramid(pyramid_level) {
                tracing::warn!(
                    backend = "libcamera",
                    pyramid_level,
                    tdn_enabled = enable_tdn_output,
                    "libcamera pyramid companion disabled: second output unavailable for this request"
                );
            }
            let build = |role, code: FourCc, second| {
                configure_streams(&cam, role, second, code, size, depth_u32)
            };

            let desired_format = if emulate_rgb24 {
                FourCc::NV12
            } else {
                libcamera_code
            };
            let (mut cfgs, mut status) = build(role, desired_format, second)?;
            if matches!(status, CameraConfigurationStatus::Invalid)
                && matches!(second, SecondStream::Pyramid(_))
            {
                tracing::warn!(
                    backend = "libcamera",
                    "libcamera pyramid companion rejected by the pipeline; continuing without it"
                );
                second = SecondStream::None;
                (cfgs, status) = build(role, desired_format, second)?;
            }
            if matches!(status, CameraConfigurationStatus::Invalid) && emulate_rgb24 {
                (cfgs, status) = build(role, FourCc::YUYV, second)?;
            }
            // GREY/R8 on a colour sensor: libcamera rewrites the request to raw Bayer. Capture
            // processed YUV420 instead and expose its Y plane as a zero-copy GREY frame.
            let wants_luma = matches!(requested_code, FourCc::GREY | FourCc::R8);
            let mut luma_view = false;
            if wants_luma
                && (matches!(status, CameraConfigurationStatus::Invalid)
                    || cfgs
                        .get(0)
                        .map(|cfg| map_pixel_format_to_fourcc(cfg.get_pixel_format()))
                        != Some(requested_code))
            {
                let processed = util::processed_stream_role(libcamera_config.processed_stream_role);
                let luma_second = if pyramid_level > 0 && !enable_tdn_output {
                    SecondStream::Pyramid(pyramid_level)
                } else {
                    second
                };
                let (mut luma_cfgs, mut luma_status) = build(processed, FourCc::YU12, luma_second)?;
                if matches!(luma_status, CameraConfigurationStatus::Invalid)
                    && luma_second != second
                {
                    (luma_cfgs, luma_status) = build(processed, FourCc::YU12, second)?;
                } else {
                    second = luma_second;
                }
                if !matches!(luma_status, CameraConfigurationStatus::Invalid) {
                    (cfgs, status) = (luma_cfgs, luma_status);
                    luma_view = true;
                }
            }
            if matches!(status, CameraConfigurationStatus::Invalid) {
                return Err(CaptureError::Backend("config invalid".into()));
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
            let output_format = if emulate_rgb24 || luma_view {
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

            let emulation = Emulation::for_request(
                emulate_rgb24,
                validated_code,
                requested_code,
                validated_res,
            );
            let stream = cfg
                .stream()
                .ok_or_else(|| CaptureError::Backend("missing stream".into()))?;
            let has_second = second != SecondStream::None;
            let tdn_stream = if has_second {
                cfgs.get(1).and_then(|cfg| cfg.stream())
            } else {
                None
            };
            let cfg_stride = cfg.get_stride() as usize;
            let tdn_stride = if has_second {
                cfgs.get(1).map(|cfg| cfg.get_stride() as usize)
            } else {
                None
            };
            let companion = match second {
                SecondStream::Pyramid(level) => cfgs.get(1).and_then(|cfg| {
                    let size = cfg.get_size();
                    let res = Resolution::new(size.width, size.height)?;
                    let code = map_pixel_format_to_fourcc(cfg.get_pixel_format());
                    Some((
                        level,
                        MediaFormat::new(code, res, mode_for_thread.format.color),
                    ))
                }),
                _ => None,
            };
            tracing::debug!(
                backend = "libcamera",
                camera_id = %id_for_thread,
                requested_fourcc = ?requested_code,
                validated_fourcc = ?validated_code,
                output_fourcc = ?output_format.code,
                width = validated_res.width.get(),
                height = validated_res.height.get(),
                stride_bytes = cfg_stride,
                tdn_enabled = enable_tdn_output,
                luma_view,
                pyramid_companion = ?companion,
                tdn_stride_bytes = tdn_stride,
                stream_role = ?libcamera_config.processed_stream_role,
                "libcamera negotiated capture format"
            );
            let heap_path = heap::select_heap(
                libcamera_config.buffer_memory,
                util::is_rpi_pisp_sensor_i2c(&id_for_thread),
            );
            let mut alloc = libcamera::framebuffer_allocator::FrameBufferAllocator::new(&cam);
            let primary = heap::allocate_stream_buffers(&mut alloc, &stream, heap_path.as_deref())
                .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;
            let tdn = if let Some(tdn_stream) = &tdn_stream {
                Some(
                    heap::allocate_stream_buffers(&mut alloc, tdn_stream, heap_path.as_deref())
                        .map_err(|e| classify_libcamera_backend_message(e.to_string()))?,
                )
            } else {
                None
            };
            let buffer_memory: &'static str = if primary.memory.starts_with("dma-heap") {
                "dma-heap"
            } else {
                "libcamera-allocator"
            };
            tracing::debug!(
                backend = "libcamera",
                camera_id = %id_for_thread,
                buffer_count = primary.buffers.len(),
                buffer_memory = %primary.memory,
                tdn_buffer_count = tdn.as_ref().map_or(0, |tdn| tdn.buffers.len()),
                "libcamera allocated capture buffers"
            );
            let primary_buffers: Vec<CaptureBuffer> = primary.buffers;
            let tdn_buffers: Option<Vec<CaptureBuffer>> = tdn.map(|tdn| tdn.buffers);
            let prefault_request_pools =
                util::prefault_request_pools_enabled(libcamera_config.prefault_request_pools);
            let _primary_request_pool_lease = RequestPoolBackingLease::new(
                request_pool_tracker_for_thread,
                &framebuffer_refs(&primary_buffers),
                prefault_request_pools,
            );
            let _tdn_request_pool_lease = tdn_buffers.as_ref().map(|buffers| {
                RequestPoolBackingLease::new(
                    tdn_request_pool_tracker_for_thread,
                    &framebuffer_refs(buffers),
                    prefault_request_pools,
                )
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
                    buf.add_to(&mut req, &stream)
                        .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;
                    tdn_buf
                        .add_to(&mut req, tdn_stream)
                        .map_err(|e| classify_libcamera_backend_message(e.to_string()))?;
                    requests.push(req);
                }
            } else {
                for (i, buf) in primary_buffers.into_iter().enumerate() {
                    let mut req = cam
                        .create_request(Some(i as u64))
                        .ok_or_else(|| CaptureError::Backend("request create failed".into()))?;
                    buf.add_to(&mut req, &stream)
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
                        LIBCAMERA_FRAME_DURATION_LIMITS.0,
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
            let mapping_cache = Arc::new(backing::MappingCache::default());
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
                            .is_some_and(|since| since.elapsed() >= requeue_stall_timeout)
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
                                pending_controls_for_thread.lock().updates.clear();
                                continue;
                            }
                            let updates = {
                                let mut guard = pending_controls_for_thread.lock();
                                std::mem::take(&mut guard.updates)
                            };
                            for (id, val) in updates {
                                if !writable_controls_for_thread.contains(&id) {
                                    continue;
                                }
                                match val {
                                    Some(val) => {
                                        if id == LIBCAMERA_FRAME_DURATION_LIMITS {
                                            if let ControlValue::Int(v) = val {
                                                frame_duration = Some(v as i64);
                                            }
                                        } else {
                                            control_state.insert(id, val);
                                        }
                                    }
                                    None => {
                                        if id == LIBCAMERA_FRAME_DURATION_LIMITS {
                                            frame_duration = None;
                                        } else {
                                            control_state.remove(&id);
                                        }
                                    }
                                }
                            }
                        }
                        ControlMessage::Get(id, resp_tx) => {
                            let pending = pending_controls_for_thread.lock().get(&id);
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

                match req_rx.recv_timeout(request_poll) {
                    Ok(req) => {
                        let timing = read_request_metadata(&req, &mut readback_state);

                        let (framebuffer, active_stride): (&dyn AsFrameBuffer, usize) =
                            if let Some(tdn_stream) =
                                tdn_stream.as_ref().filter(|_| companion.is_none())
                            {
                                match request_buffer(&req, tdn_stream) {
                                    Some(fb) => (fb, tdn_stride.unwrap_or(cfg_stride)),
                                    None => match request_buffer(&req, &stream) {
                                        Some(fb) => (fb, cfg_stride),
                                        None => break,
                                    },
                                }
                            } else {
                                match request_buffer(&req, &stream) {
                                    Some(fb) => (fb, cfg_stride),
                                    None => break,
                                }
                            };
                        let frame_parts =
                            match completed_frame_parts(framebuffer, wire_format, active_stride) {
                                Ok(parts) => parts,
                                Err(err) => {
                                    failure = Some(err);
                                    break;
                                }
                            };
                        // The ISP's second output from the same request: same exposure/timestamp.
                        let companion_parts = companion.and_then(|(level, format)| {
                            let fb = request_buffer(&req, tdn_stream.as_ref()?)?;
                            let stride = tdn_stride.unwrap_or(0);
                            match completed_frame_parts(fb, format, stride) {
                                Ok(parts) => Some((level, format, parts)),
                                Err(err) => {
                                    tracing::debug!(backend = "libcamera", error = %err, "pyramid companion unavailable");
                                    None
                                }
                            }
                        });
                        let backing = LibcameraBacking::new(
                            req,
                            ret_tx.clone(),
                            frame_parts.plane_views,
                            mapping_cache.clone(),
                            shutting_down.clone(),
                            outstanding_backings_for_thread.clone(),
                            outstanding_lease_tracker_for_thread.clone(),
                            mapped_lease_tracker_for_thread.clone(),
                        );
                        let timestamp = timing.sensor_timestamp.unwrap_or(frame_parts.timestamp);
                        match timing.frame_duration_ns {
                            Some(duration) => {
                                sequence_tracker.observe_timestamp(timestamp, duration)
                            }
                            None => sequence_tracker.observe(frame_parts.sequence),
                        }
                        // One offset for the frame and its companion so their timestamps match.
                        let conversion = timestamp_clock.conversion_from(TimestampClock::Boottime);
                        let meta = FrameMeta::new(wire_format, timestamp)
                            .with_backend(BackendFrameMeta::Libcamera(LibcameraFrameMeta {
                                sequence: frame_parts.sequence,
                                buffer_memory,
                            }))
                            .with_capture_instant(std::time::Instant::now())
                            .with_sensor_latency(TimestampClock::Boottime)
                            .in_clock(TimestampClock::Boottime, conversion)
                            .with_transition(ResidencyTransition {
                                from: FrameResidency::Dmabuf,
                                to: FrameResidency::Dmabuf,
                                reason: ResidencyTransitionReason::Capture,
                                copied: false,
                            });
                        let companion_frame = companion_parts.map(|(level, format, parts)| {
                            let meta = FrameMeta::new(format, timestamp)
                                .with_backend(BackendFrameMeta::Libcamera(LibcameraFrameMeta {
                                    sequence: parts.sequence,
                                    buffer_memory,
                                }))
                                .in_clock(TimestampClock::Boottime, conversion);
                            let sibling = backing.sibling(parts.plane_views);
                            (
                                level,
                                FrameLease::from_external(meta, parts.layouts, sibling),
                            )
                        });
                        let frame = FrameLease::from_external(meta, frame_parts.layouts, backing);
                        let frame = match attach_companion(frame, companion_frame, luma_view) {
                            Ok(frame) => frame,
                            Err(err) => {
                                failure = Some(err);
                                break;
                            }
                        };
                        let frame = if luma_view {
                            match frame.into_luma() {
                                Ok(frame) => frame,
                                Err(err) => {
                                    failure = Some(CaptureError::Backend(err.to_string()));
                                    break;
                                }
                            }
                        } else if let Some(emulation) = &emulation {
                            match emulation.process(frame) {
                                Ok(out) => out,
                                Err(err) => {
                                    failure = Some(err);
                                    break;
                                }
                            }
                        } else {
                            frame
                        };
                        if enqueue_capture_frame(&tx, frame, "libcamera", queue_send_timeout) {
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

        if util::stop_when_idle_enabled(libcamera_config.stop_when_idle)
            && !enable_tdn_output_for_thread
            && wait_for_backings_to_drain(
                &outstanding_backings_for_thread,
                idle_drain_timeout,
                idle_drain_poll,
            )
        {
            let _ = styx_libcamera::try_stop_if_idle();
        }

        if let Err(e) = res {
            record_worker_error(&worker_error_for_thread, &e);
            tracing::error!(backend = "libcamera", error = %e, "libcamera capture worker failed");
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
            response_timeout: Duration::from_millis(libcamera_config.control_response_timeout_ms),
        },
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(WorkerHandle::Thread(worker)),
        aux_workers: Vec::new(),
        libcamera_idle_stop_allowed: !enable_tdn_output_for_thread,
        libcamera_stop_when_idle: libcamera_config.stop_when_idle,
        metrics: StageMetrics::default(),
        external_backings: vec![
            request_pool_tracker,
            tdn_request_pool_tracker,
            outstanding_lease_tracker,
            mapped_lease_tracker,
        ],
        worker_error,
        control_error: Arc::new(Mutex::new(None)),
        shutdown_stats: Default::default(),
        retry_metrics: Default::default(),
        sequence_gaps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_worker_error_keeps_last_failure_without_camera_hardware() {
        let worker_error = Mutex::new(None);
        let err = CaptureError::Backend("request loop failed".into());

        record_worker_error(&worker_error, &err);

        let stored = worker_error.lock().clone();
        assert_eq!(
            stored.as_ref().map(ToString::to_string),
            Some(err.to_string())
        );
    }
}
