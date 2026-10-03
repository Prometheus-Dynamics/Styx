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
    queue: Option<super::handle::CaptureQueue>,
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
    let (tx, rx) = queue.unwrap_or_else(|| {
        styx_core::queue::bounded_with(queue_depth, capture_tunables.queue_overflow)
    });
    let (stop_tx, stop_rx) = mpsc::channel();
    let frame_interval = interval
        .map(|interval| Duration::from_secs_f32(1.0 / interval.fps().max(1.0)))
        .unwrap_or_else(|| Duration::from_millis(10))
        .max(Duration::from_millis(1));
    let idle_poll = Duration::from_millis(capture_tunables.idle_poll_ms);
    let timestamp_clock = capture_tunables.timestamp_clock;
    #[cfg(target_os = "linux")]
    let mut imported = imported::Slots::claim(config, &mode);
    let worker = thread::spawn(move || {
        tracing::debug!(backend = "virtual", "capture worker started");
        let start = std::time::Instant::now();
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            #[cfg(target_os = "linux")]
            let next = match imported.as_mut() {
                Some(slots) => slots.next_frame(),
                None => capture.next_frame(),
            };
            #[cfg(not(target_os = "linux"))]
            let next = capture.next_frame();
            if let Some(mut frame) = next {
                let (timestamp, clock) = timestamp_clock.stamp_now(start.elapsed());
                let meta = frame.meta_mut();
                meta.timestamp = timestamp;
                meta.clock = Some(clock);
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
        sequence_gaps: Default::default(),
    })
}

/// Frames "captured" into the caller's buffers (see [`super::import`]): the virtual camera
/// writes nothing, so a frame is a buffer taken until the frame is dropped.
#[cfg(target_os = "linux")]
mod imported {
    use std::sync::mpsc::{Receiver, Sender, channel};

    use styx_core::prelude::*;

    use crate::capture_api::StyxConfig;
    use crate::capture_api::import::Claim;
    use crate::prelude::Mode;

    pub(super) struct Slots {
        claim: Claim,
        free: Vec<usize>,
        returned: (Sender<usize>, Receiver<usize>),
    }

    impl Slots {
        /// The config's buffers, when they are for `mode`'s frames and no other capture has them.
        pub(super) fn claim(config: &StyxConfig, mode: &Mode) -> Option<Self> {
            let buffers = config.capture_buffers.as_ref()?;
            let format = buffers.format();
            if (format.code, format.resolution) != (mode.format.code, mode.format.resolution) {
                return None;
            }
            let claim = buffers.claim()?;
            Some(Self {
                free: (0..buffers.len()).rev().collect(),
                claim,
                returned: channel(),
            })
        }

        /// A frame in a free buffer; `None` while every buffer is held.
        pub(super) fn next_frame(&mut self) -> Option<FrameLease> {
            self.free.extend(self.returned.1.try_iter());
            let index = self.free.pop()?;
            let buffers = self.claim.buffers();
            let returned = self.returned.0.clone();
            let meta =
                FrameMeta::new(buffers.format(), 0).with_capture_instant(std::time::Instant::now());
            Some(buffers.frame(
                index,
                meta,
                buffers.planes().iter().copied().collect(),
                move || {
                    let _ = returned.send(index);
                },
            ))
        }
    }
}
