use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use styx_capture::CaptureSource;
use styx_capture::virtual_backend::VirtualCapture;
use styx_core::prelude::*;

use crate::BackendKind;
use crate::capture_api::handle::{WorkerHandle, enqueue_capture_frame};
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, StyxConfig,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};

pub(super) fn start_virtual(
    mode: Mode,
    interval: Option<Interval>,
    descriptor: CaptureDescriptor,
    config: &StyxConfig,
) -> Result<CaptureHandle, CaptureError> {
    let capture_tunables = config.capture_tunables();
    let pool_limits = capture_tunables.pool_limits(4, 1 << 20, 8);
    #[cfg(target_os = "linux")]
    let capture = {
        let pool =
            SharedBufferPool::with_limits(pool_limits.min, pool_limits.bytes, pool_limits.spare)
                .map_err(|err| {
                    CaptureError::Backend(format!("virtual shared pool failed: {err}"))
                })?;
        VirtualCapture::new_shared(mode.clone(), pool, 3)
    };
    #[cfg(not(target_os = "linux"))]
    let capture = {
        let pool = BufferPool::with_limits(pool_limits.min, pool_limits.bytes, pool_limits.spare);
        VirtualCapture::new(mode.clone(), pool, 3)
    };
    let queue_depth = capture_tunables.queue_depth;
    let (tx, rx) = bounded(queue_depth);
    let (stop_tx, stop_rx) = mpsc::channel();
    let frame_interval = interval
        .map(|interval| Duration::from_secs_f32(1.0 / interval.fps().max(1.0)))
        .unwrap_or_else(|| Duration::from_millis(10))
        .max(Duration::from_millis(1));
    let idle_poll = Duration::from_millis(capture_tunables.idle_poll_ms);
    let worker = thread::spawn(move || {
        tracing::debug!(backend = "virtual", "capture worker started");
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            if let Some(frame) = capture.next_frame() {
                if enqueue_capture_frame(&tx, frame, "virtual", frame_interval) {
                    break;
                }
                if stop_rx.recv_timeout(frame_interval).is_ok() {
                    break;
                }
            } else if stop_rx.recv_timeout(idle_poll).is_ok() {
                break;
            }
        }
        tracing::debug!(backend = "virtual", "capture worker stopped");
    });
    Ok(CaptureHandle {
        backend: BackendKind::Virtual,
        control: ControlPlane::Virtual,
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
        external_backings: Vec::new(),
        worker_error: Arc::new(parking_lot::Mutex::new(None)),
        control_error: Arc::new(parking_lot::Mutex::new(None)),
        shutdown_stats: Default::default(),
        retry_metrics: Default::default(),
    })
}
