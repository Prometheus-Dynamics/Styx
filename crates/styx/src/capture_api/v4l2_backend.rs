use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::{SmallVec, smallvec};
use styx_core::prelude::*;
use styx_kernel::v4l2::{BufType, Format, VideoDevice};

use crate::capture_api::controls::apply_v4l2_controls;
use crate::capture_api::handle::{CaptureQueue, enqueue_capture_frame, record_worker_error};
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, StyxConfig, WorkerHandle,
};
use crate::metrics::{ExternalBackingTracker, StageMetrics};
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

struct V4l2MmapBacking {
    manager: Arc<V4l2MmapManager>,
    recycle_tx: Sender<usize>,
    index: Mutex<Option<usize>>,
    tracker: Arc<ExternalBackingTracker>,
    bytes: usize,
}

impl V4l2MmapBacking {
    fn new(
        manager: Arc<V4l2MmapManager>,
        recycle_tx: Sender<usize>,
        index: usize,
        tracker: Arc<ExternalBackingTracker>,
        bytes: usize,
    ) -> Arc<Self> {
        tracker.acquire(bytes);
        Arc::new(Self {
            manager,
            recycle_tx,
            index: Mutex::new(Some(index)),
            tracker,
            bytes,
        })
    }
}

impl ExternalBacking for V4l2MmapBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        // Every plane of a single-planar buffer lives in the same mapping; plane layouts carry
        // the per-plane offsets.
        match index {
            0..=2 => {
                // The manager is held by `Arc` inside this backing, so the mmap remains alive
                // while any `FrameLease` borrowing this external backing exists.
                let buffer_index = *self.index.lock();
                self.manager.mapped_plane(buffer_index?)
            }
            _ => None,
        }
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.bytes)
    }

    fn backing_kind(&self) -> &'static str {
        "v4l2_mmap"
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let Some(index) = *self.index.lock() else {
            return Err(FrameExportError::InvalidDescriptor);
        };
        let fd = self
            .manager
            .export_dmabuf(index)
            .map_err(FrameExportError::Fd)?;
        Ok(Some(FrameBackingExport::DmabufPlanes {
            planes: vec![FrameFdPlane {
                fd,
                offset: 0,
                len: self.bytes,
            }],
        }))
    }
}

impl Drop for V4l2MmapBacking {
    fn drop(&mut self) {
        self.tracker.release(self.bytes);
        if let Some(index) = self.index.lock().take() {
            // Recycle only when the final external backing reference drops. This prevents the
            // worker from requeueing/unmapping a V4L2 buffer while a `FrameLease` still exposes it.
            let _ = self.recycle_tx.send(index);
        }
    }
}

/// Copy each plane of a single-planar YUV buffer into its own owned buffer.
fn copy_planes(meta: FrameMeta, src: &[u8], planes: &[PlaneLayout]) -> FrameLease {
    let largest = planes.iter().map(|plane| plane.len).max().unwrap_or(0);
    let pool = BufferPool::with_limits(planes.len(), largest, planes.len());
    let mut buffers = SmallVec::<[BufferLease; 3]>::new();
    let mut layouts = SmallVec::<[PlaneLayout; 3]>::new();
    for plane in planes {
        let mut lease = pool.lease();
        lease.resize(plane.len);
        lease
            .as_mut_slice()
            .copy_from_slice(&src[plane.offset..plane.offset + plane.len]);
        buffers.push(lease);
        layouts.push(PlaneLayout {
            offset: 0,
            len: plane.len,
            stride: plane.stride,
        });
    }
    FrameLease::multi_plane(meta, buffers, layouts)
}

fn drain_recycled_buffers(manager: &V4l2MmapManager, recycle_rx: &Receiver<usize>) {
    while let Ok(index) = recycle_rx.try_recv() {
        let _ = manager.recycle(index);
    }
}

pub(super) fn start_v4l2(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
    config: &StyxConfig,
    queue: Option<CaptureQueue>,
) -> Result<CaptureHandle, CaptureError> {
    let path = match &backend.handle {
        BackendHandle::V4l2 { path } => path.clone(),
        _ => return Err(CaptureError::Backend("v4l2 path missing".into())),
    };

    let backend_err = |e: styx_kernel::Error| CaptureError::Backend(e.to_string());
    let dev = VideoDevice::open(&path).map_err(backend_err)?;

    let fmt = negotiate_format(&dev, &mode).map_err(backend_err)?;
    let negotiated_code = FourCc::from(fmt.fourcc.to_u32());
    let negotiated_resolution = Resolution::new(fmt.width, fmt.height)
        .ok_or_else(|| CaptureError::Backend("v4l2 negotiated zero-sized frame".into()))?;
    let negotiated_format =
        MediaFormat::new(negotiated_code, negotiated_resolution, mode.format.color);
    let mode = Mode {
        id: mode.id,
        format: negotiated_format,
        intervals: mode.intervals,
        interval_stepwise: mode.interval_stepwise,
    };

    if let Some(iv) = interval {
        let interval = styx_kernel::Fraction::new(iv.numerator.get(), iv.denominator.get());
        dev.set_frame_interval(BufType::VideoCapture, interval)
            .map_err(backend_err)?;
    }

    if !controls.is_empty() {
        apply_v4l2_controls(&path, &controls)?;
    }

    let width = fmt.width as usize;
    let height = fmt.height as usize;
    let encoded = is_encoded_bitstream(mode.format.code);
    let min_stride = min_stride_for_fourcc(mode.format.code, width);
    let negotiated_stride_bytes = if encoded {
        0
    } else if fmt.bytes_per_line > 0 {
        (fmt.bytes_per_line as usize).max(min_stride)
    } else {
        min_stride.max(1)
    };
    let negotiated_size = fmt.size_image as usize;
    let frame_capacity = if encoded {
        negotiated_size
            .max(256 * 1024)
            .max(width.saturating_mul(height))
    } else {
        height
            .saturating_mul(negotiated_stride_bytes)
            .max(negotiated_size)
            .max(width.saturating_mul(fmt.height as usize).saturating_mul(3))
    };
    tracing::debug!(
        backend = "v4l2",
        path = %path,
        width = fmt.width,
        height = fmt.height,
        fourcc = ?mode.format.code,
        stride_bytes = negotiated_stride_bytes,
        buffer_size = negotiated_size,
        frame_capacity,
        encoded,
        "v4l2 negotiated capture format"
    );
    let capture_tunables = config.capture_tunables();
    let timestamp_clock = capture_tunables.timestamp_clock;
    let sequence_gaps = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let worker_error = Arc::new(Mutex::new(None));
    let worker_error_for_thread = worker_error.clone();
    let mut sequence_tracker = crate::metrics::SequenceGapTracker::new(sequence_gaps.clone());
    let v4l2_config = config.v4l2_config();
    let pool_limits = capture_tunables.pool_limits(4, frame_capacity, 8);
    let manager = V4l2MmapManager::new(
        dev,
        BufType::VideoCapture,
        u32::try_from(capture_tunables.queue_depth + capture_tunables.extra_buffers)
            .unwrap_or(4)
            .clamp(3, 16),
        Duration::from_millis(v4l2_config.mmap_poll_ms),
    )
    .map(Arc::new)
    .map_err(backend_err)?;
    let queue_depth = capture_tunables.queue_depth;
    let (tx, rx) = queue.unwrap_or_else(|| {
        styx_core::queue::bounded_with(queue_depth, capture_tunables.queue_overflow)
    });
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<usize>();
    let mode_clone = mode.clone();
    let backing_tracker = Arc::new(ExternalBackingTracker::new("v4l2_mmap"));
    let manager_for_worker = Arc::clone(&manager);
    let tracker_for_worker = Arc::clone(&backing_tracker);
    let worker = thread::spawn(move || {
        let send_timeout = Duration::from_millis(v4l2_config.send_timeout_ms);
        let error_backoff = Duration::from_millis(v4l2_config.error_backoff_ms);
        let zero_copy_requested = supports_v4l2_mmap_zero_copy(mode_clone.format.code);
        let shared_pool =
            SharedBufferPool::with_limits(pool_limits.min, pool_limits.bytes, pool_limits.spare);
        let height = mode_clone.format.resolution.height.get() as usize;
        let width = mode_clone.format.resolution.width.get() as usize;
        loop {
            drain_recycled_buffers(&manager_for_worker, &recycle_rx);
            if stop_rx.try_recv().is_ok() {
                drain_recycled_buffers(&manager_for_worker, &recycle_rx);
                let _ = manager_for_worker.stop_stream();
                break;
            }
            match manager_for_worker.dequeue() {
                Ok(Some((index, meta))) => {
                    let mapped_len = manager_for_worker.mapped_bytes(index).unwrap_or_default();
                    let bytes_used = meta.bytes_used().min(mapped_len);
                    let Some(layout_plan) = plan_v4l2_single_plane_layout(
                        mode_clone.format.code,
                        width,
                        height,
                        negotiated_stride_bytes,
                        negotiated_size,
                        mapped_len,
                        bytes_used,
                    ) else {
                        let _ = manager_for_worker.recycle(index);
                        continue;
                    };
                    let zero_copy_enabled = zero_copy_requested && layout_plan.zero_copy_safe;
                    let ts = meta.timestamp.as_nanos().min(u64::MAX as u128) as u64;
                    // V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC within V4L2_BUF_FLAG_TIMESTAMP_MASK.
                    let monotonic = meta.flags.bits() & 0xE000 == 0x2000;
                    sequence_tracker.observe(meta.sequence);
                    let encoded = is_encoded_bitstream(mode_clone.format.code);
                    let frame_meta = FrameMeta::new(mode_clone.format, ts)
                        .with_capture_instant(std::time::Instant::now());
                    let clock = TimestampClock::Monotonic;
                    let frame_meta = match monotonic {
                        true => frame_meta
                            .with_sensor_latency(clock)
                            .in_clock(clock, timestamp_clock.conversion_from(clock)),
                        false => frame_meta,
                    };
                    let meta = frame_meta
                        .with_transition(ResidencyTransition {
                            from: match (zero_copy_enabled, encoded) {
                                (true, _) => FrameResidency::HostExternal,
                                (false, true) => FrameResidency::CompressedPacket,
                                (false, false) => FrameResidency::HostOwned,
                            },
                            to: match encoded && !zero_copy_enabled {
                                true => FrameResidency::CompressedPacket,
                                false => FrameResidency::HostExternal,
                            },
                            reason: ResidencyTransitionReason::Capture,
                            copied: !zero_copy_enabled,
                        })
                        .with_backend(BackendFrameMeta::V4l2(V4l2FrameMeta {
                            sequence: meta.sequence,
                            bytes_used: meta.bytes_used() as u32,
                            field: meta.field,
                            flags: meta.flags.bits(),
                            zero_copy: zero_copy_enabled,
                        }));
                    let layout = layout_plan.layout;
                    let frame = if zero_copy_enabled {
                        let backing = V4l2MmapBacking::new(
                            Arc::clone(&manager_for_worker),
                            recycle_tx.clone(),
                            index,
                            Arc::clone(&tracker_for_worker),
                            bytes_used,
                        );
                        FrameLease::from_external(meta, layout_plan.planes.clone(), backing)
                    } else if layout_plan.planes.len() > 1 {
                        let frame = manager_for_worker
                            .mapped_plane(index)
                            .map(|src| copy_planes(meta, src, &layout_plan.planes));
                        let _ = manager_for_worker.recycle(index);
                        match frame {
                            Some(frame) => frame,
                            None => continue,
                        }
                    } else {
                        let Ok(pool) = &shared_pool else {
                            let _ = manager_for_worker.recycle(index);
                            continue;
                        };
                        let Ok(mut lease) = pool.lease() else {
                            let _ = manager_for_worker.recycle(index);
                            continue;
                        };
                        if lease.try_resize(bytes_used).is_err() {
                            let _ = manager_for_worker.recycle(index);
                            continue;
                        }
                        if let Some(src) = manager_for_worker.mapped_plane(index) {
                            lease.as_mut_slice()[..bytes_used].copy_from_slice(&src[..bytes_used]);
                        }
                        let _ = manager_for_worker.recycle(index);
                        match FrameLease::single_plane_shared(
                            meta,
                            lease,
                            layout.len,
                            layout.stride,
                        ) {
                            Ok(frame) => frame,
                            Err(_) => continue,
                        }
                    };
                    if enqueue_capture_frame(&tx, frame, "v4l2", send_timeout) {
                        let _ = manager_for_worker.stop_stream();
                        break;
                    }
                }
                // Timeouts are expected due to the short poll timeout.
                Ok(None) => {}
                Err(err) if err.is_no_device() => {
                    // The device is gone; retrying the dequeue cannot succeed.
                    let err = CaptureError::Disconnected(to_io(err).to_string());
                    tracing::warn!(backend = "v4l2", error = %err, "v4l2 device disconnected");
                    record_worker_error(&worker_error_for_thread, &err);
                    break;
                }
                Err(_) => thread::sleep(error_backoff),
            }
        }
    });

    Ok(CaptureHandle {
        backend: BackendKind::V4l2,
        control: ControlPlane::V4l2 { path },
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(WorkerHandle::Thread(worker)),
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: vec![backing_tracker],
        worker_error,
        control_error: Arc::new(Mutex::new(None)),
        shutdown_stats: Default::default(),
        retry_metrics: Default::default(),
        sequence_gaps,
    })
}

/// Sets the capture format to `mode`'s size and pixel format, keeping the node's other
/// format fields; returns the format the driver chose.
fn negotiate_format(dev: &VideoDevice, mode: &Mode) -> styx_kernel::Result<PixFormat> {
    const PIX_FMT_FLAG_PREMUL_ALPHA: u32 = 1;
    let Format::Single(mut pix) = dev.format(BufType::VideoCapture)? else {
        return Err(styx_kernel::Error::Invalid(
            "v4l2 capture node has no single-planar format".into(),
        ));
    };
    pix.width = mode.format.resolution.width.get();
    pix.height = mode.format.resolution.height.get();
    pix.fourcc = styx_kernel::FourCc(mode.format.code.to_u32());
    pix.ycbcr_enc = 0;
    pix.flags &= PIX_FMT_FLAG_PREMUL_ALPHA;
    dev.set_format(BufType::VideoCapture, &Format::Single(pix))?;
    match dev.format(BufType::VideoCapture)? {
        Format::Single(pix) => Ok(pix),
        _ => Err(styx_kernel::Error::Invalid(
            "v4l2 capture node has no single-planar format".into(),
        )),
    }
}

mod layout;
mod mmap;
#[cfg(test)]
mod tests;
mod yuv_layout;

use mmap::{V4l2MmapManager, to_io};
use styx_kernel::v4l2::PixFormat;

use layout::{
    is_encoded_bitstream, min_stride_for_fourcc, plan_v4l2_single_plane_layout,
    supports_v4l2_mmap_zero_copy,
};
