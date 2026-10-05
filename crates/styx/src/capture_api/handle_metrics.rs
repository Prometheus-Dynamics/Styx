//! A capture's per-camera metrics: where backends record frames and handles hand them out.

use std::time::{Duration, Instant};

use styx_capture::prelude::*;

use super::handle::{CaptureHandle, enqueue_capture_frame};
use crate::metrics::{Attached, CameraMetrics, CaptureMetrics};

/// Record `frame` in `live`, then send it to the consumer queue as
/// [`enqueue_capture_frame`] does; `true` when the queue is closed.
#[inline]
pub(crate) fn deliver(
    live: &CaptureMetrics,
    tx: &BoundedTx<FrameLease>,
    frame: FrameLease,
    backend: &'static str,
    timeout: Duration,
) -> bool {
    live.frame(frame.meta());
    enqueue_capture_frame(tx, frame, backend, timeout)
}

impl CaptureHandle {
    /// Health and performance of this capture: frames, measured and configured frame rate,
    /// drops by cause, sensor-to-delivery latency, ISP and CPU time, 3A state, restarts,
    /// buffers, consumers. Cheap to call (reads counters; a few `/proc` reads for CPU time).
    /// [`crate::metrics::snapshot`] gives the same for every capture of the process.
    pub fn camera_metrics(&self) -> CameraMetrics {
        self.attach_metrics();
        self.live.snapshot()
    }

    /// The live metrics this capture records into (cheap to clone; take a
    /// [`CaptureMetrics::snapshot`] whenever needed, also after the handle is gone).
    pub fn live_metrics(&self) -> CaptureMetrics {
        self.attach_metrics();
        self.live.clone()
    }

    /// Give the metrics what snapshots taken without the handle need.
    pub(crate) fn attach_metrics(&self) {
        let live = &self.live;
        live.0.attached.get_or_init(|| Attached {
            queue: self.rx.clone(),
            worker_error: self.worker_error.clone(),
            retry: self.retry_metrics.clone(),
            external: self.external_backings.clone(),
            gaps: self.sequence_gaps.clone(),
        });
        if let Ok(mut info) = live.0.info.lock()
            && info.backend.is_empty()
        {
            let res = self.mode.format.resolution;
            info.backend = self.backend.to_string();
            info.mode = format!("{} {}x{}", self.mode.format.code, res.width, res.height);
            info.configured_fps = self.interval.map(|i| i.fps() as f64);
        }
    }

    /// The handle a request returns: its metrics listed in [`crate::metrics::snapshot`] under
    /// `name`.
    pub(crate) fn published(self, name: &str) -> Self {
        self.attach_metrics();
        if let Ok(mut info) = self.live.0.info.lock() {
            info.name = name.to_string();
            crate::trace::info!(
                camera = name,
                capture = self.live.0.id,
                backend = %info.backend,
                mode = %info.mode,
                fps = info.configured_fps,
                "capture started"
            );
        }
        crate::metrics::register(&self.live);
        self
    }

    /// A frame the consumer took after waiting since `start`.
    #[inline]
    pub(super) fn took(&self, start: Instant, frame: &FrameLease) {
        self.metrics.record(start.elapsed());
        self.live.received(frame.meta());
    }
}
