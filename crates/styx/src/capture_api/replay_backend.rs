//! Replays a `.styxrec` recording as a camera (see [`crate::replay`]).

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_capture::prelude::*;

use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle, enqueue_capture_frame};
use super::request::CaptureError;
use super::tunables::StyxConfig;
use crate::metrics::StageMetrics;
use crate::replay::{ReplayError, ReplayPacing, open_recording};
use crate::{BackendHandle, BackendKind, ProbedBackend};

pub(super) fn start_replay(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    descriptor: CaptureDescriptor,
    config: &StyxConfig,
    queue: Option<CaptureQueue>,
) -> Result<CaptureHandle, CaptureError> {
    let BackendHandle::Replay {
        path,
        pacing,
        loop_forever,
    } = &backend.handle
    else {
        return Err(CaptureError::BackendUnavailable(BackendKind::Replay));
    };
    let (path, pacing, loop_forever) = (path.clone(), *pacing, *loop_forever);
    let replay_error = |err: ReplayError| CaptureError::Backend(format!("replay: {err}"));
    // Fail at start, not in the worker, for a missing or foreign file.
    open_recording(&path).map_err(replay_error)?;

    let capture = config.capture_tunables();
    // Unpaced replay delivers every frame, so it waits for room instead of dropping.
    let overflow = match pacing {
        ReplayPacing::Realtime => capture.queue_overflow,
        ReplayPacing::Unpaced => QueueOverflow::Backpressure,
    };
    let (tx, rx) = queue
        .unwrap_or_else(|| styx_core::queue::bounded_with(capture.queue_depth.max(1), overflow));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let worker_error = Arc::new(Mutex::new(None));
    let worker_error_for_thread = worker_error.clone();
    let send_timeout = Duration::from_millis(capture.queue_send_timeout_ms);
    let fallback_interval = interval
        .map(|i| Duration::from_secs_f32(1.0 / i.fps().max(0.01)))
        .unwrap_or(Duration::from_millis(33));
    let worker = thread::spawn(move || {
        let mut replay = Replay {
            pacing,
            fallback_interval,
            first_timestamp: None,
            last_timestamp: 0,
            last_delta: fallback_interval,
            loop_offset: 0,
            started: Instant::now(),
        };
        let result = loop {
            match replay.play_once(&path, &tx, &stop_rx, send_timeout) {
                Ok(Played::Stopped) => break Ok(()),
                Ok(Played::Finished) if loop_forever => replay.next_loop(),
                Ok(Played::Finished) => break Ok(()),
                Err(err) => break Err(err),
            }
        };
        if let Err(err) = result {
            tracing::warn!(backend = "replay", error = %err, "replay failed");
            *worker_error_for_thread.lock() = Some(CaptureError::Backend(format!("replay: {err}")));
        }
        // End of the recording: queued frames drain, then receivers see `Closed`.
        tx.close();
    });
    Ok(CaptureHandle {
        backend: BackendKind::Replay,
        control: ControlPlane::None,
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
        worker_error,
        control_error: Arc::new(Mutex::new(None)),
        shutdown_stats: Default::default(),
        retry_metrics: Default::default(),
        sequence_gaps: Default::default(),
    })
}

enum Played {
    Finished,
    Stopped,
}

struct Replay {
    pacing: ReplayPacing,
    fallback_interval: Duration,
    /// First recorded timestamp; frames are scheduled relative to it.
    first_timestamp: Option<u64>,
    last_timestamp: u64,
    last_delta: Duration,
    /// Added to recorded timestamps so they keep increasing across loops.
    loop_offset: u64,
    started: Instant,
}

impl Replay {
    fn play_once(
        &mut self,
        path: &std::path::Path,
        tx: &BoundedTx<FrameLease>,
        stop_rx: &mpsc::Receiver<()>,
        send_timeout: Duration,
    ) -> Result<Played, ReplayError> {
        let (_, frames) = open_recording(path)?;
        for frame in frames.with_timestamp_offset(self.loop_offset) {
            let mut frame = frame?;
            let timestamp = frame.meta().timestamp;
            let first = *self.first_timestamp.get_or_insert(timestamp);
            if timestamp > self.last_timestamp && self.last_timestamp != 0 {
                self.last_delta = Duration::from_nanos(timestamp - self.last_timestamp);
            }
            self.last_timestamp = timestamp;
            if self.pacing == ReplayPacing::Realtime {
                let due = self.started + Duration::from_nanos(timestamp.saturating_sub(first));
                let wait = due.saturating_duration_since(Instant::now());
                if !wait.is_zero() && stop_rx.recv_timeout(wait).is_ok() {
                    return Ok(Played::Stopped);
                }
            }
            if stop_rx.try_recv().is_ok() {
                return Ok(Played::Stopped);
            }
            // Recorded metadata is kept; only the delivery time is new.
            frame.meta_mut().capture_instant = Some(Instant::now());
            let closed = match self.pacing {
                ReplayPacing::Realtime => enqueue_capture_frame(tx, frame, "replay", send_timeout),
                ReplayPacing::Unpaced => send_every_frame(tx, frame, stop_rx),
            };
            if closed {
                return Ok(Played::Stopped);
            }
        }
        Ok(Played::Finished)
    }

    fn next_loop(&mut self) {
        let gap = if self.last_delta.is_zero() {
            self.fallback_interval
        } else {
            self.last_delta
        };
        // The next loop's first frame follows the last one by one frame interval. The first
        // recorded timestamp was read without an offset.
        let first = self.first_timestamp.unwrap_or(0);
        self.loop_offset = self
            .last_timestamp
            .saturating_add(gap.as_nanos() as u64)
            .saturating_sub(first);
    }
}

/// Wait for room rather than dropping; returns `true` when the replay should stop.
fn send_every_frame(
    tx: &BoundedTx<FrameLease>,
    mut frame: FrameLease,
    stop_rx: &mpsc::Receiver<()>,
) -> bool {
    loop {
        match tx.send_timeout(frame, Duration::from_millis(50)) {
            SendWaitOutcome::Ok => return false,
            SendWaitOutcome::Closed(_) => return true,
            SendWaitOutcome::Timeout(back) => {
                if stop_rx.try_recv().is_ok() {
                    return true;
                }
                frame = back;
            }
        }
    }
}
