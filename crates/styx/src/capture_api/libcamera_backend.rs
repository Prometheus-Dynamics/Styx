mod backing;
mod controls;
mod crop;
mod emulation;
mod frame;
mod heap;
mod lcraw;
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
    CaptureDescriptor, CaptureError, CaptureHandle, ControlApplyKind, ControlPlane, StyxConfig,
    TdnOutputMode, WorkerHandle,
};
use crate::metrics::{ExternalBackingTracker, StageMetrics};
use crate::prelude::{Interval, Mode, ModeId};
use crate::{BackendHandle, BackendKind, ProbedBackend};

use self::backing::{
    LibcameraBacking, RequestPoolBackingLease, RequestSlot, ShutdownGuard,
    wait_for_backings_to_drain,
};
pub use self::controls::{ControlMessage, PendingControlState};
use self::controls::{apply_control_updates, queue_with_controls, start_controls};
use self::emulation::Emulation;
use self::frame::{Completed, companion_meta, completed_frame_parts, frame_meta, sensor_clock};
use self::heap::{CaptureBuffer, request_buffer};
use self::streams::{
    SecondStream, attach_companion, build_requests, choose_second_stream, configure_streams,
    framebuffer_refs,
};
use self::util::{
    classify_libcamera_backend_message, classify_libcamera_control_apply_kind,
    map_pixel_format_to_fourcc, normalize_requested_fourcc_for_libcamera, pisp_disallowed_fourcc,
    stream_role_for_request, supports_frame_duration_limits,
};
use super::handle::{CaptureQueue, record_worker_error};
use super::handle_metrics::deliver;
pub(crate) use crop::OUTPUT_CROP;

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
    let enable_tdn_output =
        util::tdn_output(&descriptor, &requested_controls, tdn_output_mode, &id)?;

    let supports_frame_duration = supports_frame_duration_limits(&descriptor);

    let enable_tdn_output_for_thread = enable_tdn_output;
    let id_for_thread = id.clone();
    let writable_controls_for_thread = writable_controls.clone();
    let requested_controls_for_thread = requested_controls.clone();
    let interval_for_thread = interval;
    let supports_frame_duration_for_thread = supports_frame_duration;

    let capture_tunables = config.capture_tunables();
    let timestamp_clock = capture_tunables.timestamp_clock;
    let extra_buffers = capture_tunables.extra_buffers;
    let sequence_gaps = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut sequence_tracker = crate::metrics::SequenceGapTracker::new(sequence_gaps.clone());
    // Frames are recorded (and their hops stamped) as they enter the queue, as on every path.
    let live = crate::metrics::CaptureMetrics::with_sequence_gaps(sequence_gaps.clone());
    let live_worker = live.clone();
    let libcamera_config = config.libcamera_config();
    let queue_depth = capture_tunables.queue_depth;
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
        live_worker.register_thread();
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
                return Err(CaptureError::InvalidConfig(format!(
                    "{requested_code} unsupported on PiSP"
                )));
            }
            let libcamera_code = normalize_requested_fourcc_for_libcamera(requested_code);
            let emulate_rgb24 = util::is_rgb24_request(requested_code)
                && util::is_rpi_pisp_sensor_i2c(&id_for_thread);
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
            let mut second = choose_second_stream(
                enable_tdn_output,
                pyramid_level,
                libcamera_config.second_output_size,
                emulate_rgb24,
            );
            let requested_second = second;
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
                && matches!(second, SecondStream::Pyramid(_) | SecondStream::Scaled(..))
            {
                crate::trace::warn!(
                    backend = "libcamera",
                    second = ?second,
                    "libcamera second output rejected by the pipeline; continuing without it"
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
                // The second output the raw attempt may have dropped.
                let luma_second = match requested_second {
                    SecondStream::None if pyramid_level > 0 && !enable_tdn_output => {
                        SecondStream::Pyramid(pyramid_level)
                    }
                    requested => requested,
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
                return Err(match streams::offered(&cam, role) {
                    Some(offered) => CaptureError::InvalidConfig(format!(
                        "libcamera does not offer {requested_code} on this camera (offers {offered})"
                    )),
                    None => CaptureError::Backend("config invalid".into()),
                });
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
            let full = mode_for_thread.format.resolution;
            let mut crop = writable_controls_for_thread
                .contains(&crop::SCALER_CROPS)
                .then(|| crop::Crop::start(&cam, full, has_second, libcamera_config.crop));
            let overview = libcamera_config.overview;
            let companion = second.companion_kind().and_then(|kind| {
                let kind = if overview {
                    CompanionKind::Overview
                } else {
                    kind
                };
                let cfg = cfgs.get(1)?;
                let size = cfg.get_size();
                let res = Resolution::new(size.width, size.height)?;
                let code = map_pixel_format_to_fourcc(cfg.get_pixel_format());
                Some((
                    kind,
                    MediaFormat::new(code, res, mode_for_thread.format.color),
                ))
            });
            crate::trace::debug!(
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
            crate::trace::debug!(
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

            let requests = build_requests(
                &mut cam,
                primary_buffers,
                tdn_buffers,
                &stream,
                tdn_stream.as_ref(),
            )?;

            let (start_ctrls, mut frame_duration) = start_controls(
                &requested_controls_for_thread,
                requests.first(),
                interval_for_thread.filter(|_| supports_frame_duration_for_thread),
            )?;
            let mut control_state: HashMap<ControlId, ControlValue> = HashMap::new();
            let mut readback_state: HashMap<ControlId, ControlValue> = HashMap::new();
            let mut controls_enabled = true;
            for (id, val) in &requested_controls_for_thread {
                control_state.insert(*id, val.clone());
            }
            let mapping_cache = Arc::new(backing::MappingCache::default());
            // Completed requests, through a queue with room for all of them (a channel would
            // allocate as it goes).
            let (req_tx, req_rx) = styx_core::queue::bounded(requests.len() + 1);
            cam.on_request_completed(move |req| {
                let _ = req_tx.send(req);
            });
            // One return slot per request (its cookie), reused by every frame it carries.
            let slots: Vec<Arc<RequestSlot>> = (0..requests.len())
                .map(|_| {
                    RequestSlot::new(
                        shutting_down.clone(),
                        outstanding_backings_for_thread.clone(),
                    )
                })
                .collect();
            let mut sensor_timestamp_clock = None;
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
            let mut pending_requeue: Vec<libcamera::request::Request> =
                Vec::with_capacity(slots.len());
            let mut still_pending = Vec::with_capacity(slots.len());
            let mut requeue_fail_since: Option<Instant> = None;
            // Stopped while idle: requests collect in `pending_requeue` until resumed.
            let mut paused = false;
            loop {
                for mut ret_req in slots.iter().filter_map(|slot| slot.take_returned()) {
                    ret_req.reuse(ReuseFlag::REUSE_BUFFERS);
                    pending_requeue.push(ret_req);
                }
                if !paused && !pending_requeue.is_empty() {
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
                    std::mem::swap(&mut pending_requeue, &mut still_pending);
                }
                while let Ok(msg) = ctrl_rx.try_recv() {
                    match msg {
                        ControlMessage::Wake => {
                            let mut updates =
                                std::mem::take(&mut pending_controls_for_thread.lock().updates);
                            if let Some(c) = crop.as_mut() {
                                updates = c.take_updates(updates);
                            }
                            if controls_enabled {
                                apply_control_updates(
                                    updates,
                                    &writable_controls_for_thread,
                                    &mut control_state,
                                    &mut frame_duration,
                                );
                            }
                        }
                        ControlMessage::Pause(ack) => {
                            if !paused {
                                // Cancels the queued requests; they come back below and wait
                                // in `pending_requeue`.
                                if let Err(err) = cam.stop() {
                                    failure =
                                        Some(classify_libcamera_backend_message(err.to_string()));
                                    break;
                                }
                                paused = true;
                                requeue_fail_since = None;
                            }
                            let _ = ack.send(());
                        }
                        ControlMessage::Resume => {
                            if paused {
                                if let Err(err) = cam.start(None) {
                                    failure =
                                        Some(classify_libcamera_backend_message(err.to_string()));
                                    break;
                                }
                                paused = false;
                                // The gap while paused is not frames lost.
                                sequence_tracker.restart();
                            }
                        }
                        ControlMessage::Get(id, resp_tx) if id == OUTPUT_CROP && crop.is_some() => {
                            let _ = resp_tx.send(Ok(crop
                                .as_ref()
                                .map_or(ControlValue::None, crop::Crop::read)));
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

                if failure.is_some() {
                    break;
                }
                match req_rx.recv_timeout(request_poll) {
                    RecvWaitOutcome::Data(mut req)
                        if req.status() == libcamera::request::RequestStatus::Cancelled =>
                    {
                        req.reuse(ReuseFlag::REUSE_BUFFERS);
                        pending_requeue.push(req);
                    }
                    RecvWaitOutcome::Data(req) => {
                        let dequeued = Instant::now();
                        let timing = lcraw::record_metadata(
                            lcraw::entries(req.metadata()),
                            &mut readback_state,
                        );
                        let shown = crop
                            .as_mut()
                            .filter(|_| controls_enabled)
                            .and_then(|c| c.observe(&readback_state, &mut control_state));

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
                        let companion_parts = companion.and_then(|(kind, format)| {
                            let fb = request_buffer(&req, tdn_stream.as_ref()?)?;
                            let stride = tdn_stride.unwrap_or(0);
                            match completed_frame_parts(fb, format, stride) {
                                Ok(parts) => Some((kind, format, parts)),
                                Err(err) => {
                                    crate::trace::debug!(backend = "libcamera", error = %err, "second output frame unavailable");
                                    None
                                }
                            }
                        });
                        let Some(slot) = usize::try_from(req.cookie())
                            .ok()
                            .and_then(|i| slots.get(i))
                        else {
                            failure =
                                Some(CaptureError::Backend("libcamera request cookie".into()));
                            break;
                        };
                        let backing = LibcameraBacking::new(
                            slot,
                            req,
                            frame_parts.plane_views,
                            mapping_cache.clone(),
                            outstanding_lease_tracker_for_thread.clone(),
                            mapped_lease_tracker_for_thread.clone(),
                            buffer_memory == "dma-heap",
                        );
                        let timestamp = timing.sensor_timestamp.unwrap_or(frame_parts.timestamp);
                        match timing.frame_duration_ns {
                            Some(duration) => {
                                sequence_tracker.observe_timestamp(timestamp, duration)
                            }
                            None => sequence_tracker.observe(frame_parts.sequence),
                        }
                        // Decided on the first frame: the clock libcamera's timestamps are on.
                        let clock = *sensor_timestamp_clock.get_or_insert_with(|| {
                            let now = |c: TimestampClock| c.now_ns().unwrap_or(0);
                            sensor_clock(
                                timestamp,
                                now(TimestampClock::Monotonic),
                                now(TimestampClock::Boottime),
                            )
                        });
                        // One offset for the frame and its companion so their timestamps match.
                        let completed = Completed {
                            timestamp,
                            clock,
                            conversion: timestamp_clock.conversion_from(clock),
                            sequence: frame_parts.sequence,
                            buffer_memory,
                            dequeued,
                        };
                        let mut meta = frame_meta(wire_format, &completed);
                        meta.crop = shown;
                        let companion_frame = companion_parts.map(|(kind, format, parts)| {
                            let meta = companion_meta(format, &completed, parts.sequence);
                            let sibling = backing.sibling(parts.plane_views);
                            (
                                kind,
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
                        if deliver(&live_worker, &tx, frame, "libcamera", queue_send_timeout) {
                            break;
                        }
                    }
                    RecvWaitOutcome::Timeout => {
                        if stop_rx.try_recv().is_ok() {
                            shutting_down.store(true, Ordering::Release);
                            break;
                        }
                    }
                    RecvWaitOutcome::Closed => break,
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
            crate::trace::error!(backend = "libcamera", error = %e, "libcamera capture worker failed");
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
        live,
    })
}

#[cfg(test)]
mod tests;
