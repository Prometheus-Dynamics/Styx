use std::{
    mem,
    sync::{Arc, Mutex},
    time::Instant,
};

use crate::metrics::StageMetrics;
use crate::{BackendKind, ProbedBackend};

use super::control_plane::{ControlPlane, apply_control_to_plane, read_control_from_plane};
#[cfg(feature = "file-backend")]
use super::file_backend;
#[cfg(feature = "libcamera")]
use super::libcamera_backend;
#[cfg(feature = "netcam")]
use super::netcam_backend;
use super::request::{CaptureError, CaptureStartPolicy, TdnOutputMode};
#[cfg(feature = "simulation-bevy")]
use super::simulation_backend;
use super::tunables::StyxConfig;
#[cfg(feature = "v4l2")]
use super::v4l2_backend;
use super::virtual_backend;
use styx_capture::prelude::*;

pub(super) fn enqueue_capture_frame(
    tx: &BoundedTx<FrameLease>,
    frame: FrameLease,
    backend: &'static str,
    timeout: std::time::Duration,
) -> bool {
    match tx.send_timeout(frame, timeout) {
        SendWaitOutcome::Ok => false,
        SendWaitOutcome::Closed(_frame) => {
            tracing::debug!(backend, "capture output queue closed");
            true
        }
        SendWaitOutcome::Timeout(_frame) => {
            tracing::debug!(
                backend,
                drop_reason = "capture_queue_send_timeout",
                timeout_ms = timeout.as_millis() as u64,
                "dropping capture frame because output queue is full"
            );
            false
        }
    }
}

#[cfg(feature = "netcam")]
pub(super) fn record_worker_error(worker_error: &Mutex<Option<CaptureError>>, err: &CaptureError) {
    if let Ok(mut error) = worker_error.lock() {
        *error = Some(err.clone());
    }
}

/// Unified capture handle; currently backed by a bounded queue and a worker thread.
///
/// # Example
/// ```rust,no_run
/// use styx::prelude::*;
///
/// let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
/// let handle = CaptureRequest::new(&device).start()?;
/// match handle.recv() {
///     RecvOutcome::Data(frame) => println!("frame {:?}", frame.meta().format),
///     RecvOutcome::Empty => {}
///     RecvOutcome::Closed => {}
/// }
/// handle.stop();
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
pub struct CaptureHandle {
    pub(crate) backend: BackendKind,
    pub(crate) control: ControlPlane,
    pub(crate) descriptor: CaptureDescriptor,
    pub(crate) mode: Mode,
    pub(crate) interval: Option<Interval>,
    pub(crate) rx: BoundedRx<FrameLease>,
    pub(crate) stop_tx: Option<std::sync::mpsc::Sender<()>>,
    pub(super) worker: Option<WorkerHandle>,
    pub(super) aux_workers: Vec<WorkerHandle>,
    #[cfg(feature = "libcamera")]
    pub(crate) libcamera_idle_stop_allowed: bool,
    #[cfg(feature = "libcamera")]
    pub(crate) libcamera_stop_when_idle: bool,
    pub(crate) metrics: StageMetrics,
    pub(crate) external_backings: Vec<Arc<crate::metrics::ExternalBackingTracker>>,
    pub(crate) worker_error: Arc<Mutex<Option<CaptureError>>>,
    pub(crate) control_error: Arc<Mutex<Option<CaptureError>>>,
}

/// Worker handle for capture backends.
pub enum WorkerHandle {
    Thread(std::thread::JoinHandle<()>),
    #[cfg(feature = "async")]
    Async(tokio::task::JoinHandle<()>),
}

/// Blocking frame iterator returned by `CaptureHandle::frames_blocking`.
pub struct CaptureFrameIter<'a> {
    handle: &'a CaptureHandle,
    wait: std::time::Duration,
    remaining: Option<usize>,
    closed: bool,
}

impl<'a> CaptureFrameIter<'a> {
    fn new(handle: &'a CaptureHandle, wait: std::time::Duration) -> Self {
        Self {
            handle,
            wait,
            remaining: None,
            closed: false,
        }
    }

    /// Limit the iterator to at most `count` frames.
    pub fn take_frames(mut self, count: usize) -> Self {
        self.remaining = Some(count);
        self
    }
}

impl Iterator for CaptureFrameIter<'_> {
    type Item = FrameLease;

    fn next(&mut self) -> Option<Self::Item> {
        if self.closed || self.remaining == Some(0) {
            return None;
        }
        loop {
            let outcome = if self.wait.is_zero() {
                self.handle.recv_forever()
            } else {
                self.handle.recv_blocking(self.wait)
            };
            match outcome {
                RecvOutcome::Data(frame) => {
                    if let Some(remaining) = &mut self.remaining {
                        *remaining = remaining.saturating_sub(1);
                    }
                    return Some(frame);
                }
                RecvOutcome::Empty => continue,
                RecvOutcome::Closed => {
                    self.closed = true;
                    return None;
                }
            }
        }
    }
}

impl CaptureHandle {
    /// Poll the capture queue without waiting.
    ///
    /// Returns `RecvOutcome::Empty` when the queue is temporarily empty.
    pub fn recv(&self) -> RecvOutcome<FrameLease> {
        let start = Instant::now();
        match self.rx.recv() {
            RecvOutcome::Data(frame) => {
                self.metrics.record(start.elapsed());
                RecvOutcome::Data(frame)
            }
            other => other,
        }
    }

    /// Wait asynchronously until a frame arrives or capture closes.
    #[cfg(feature = "async")]
    pub async fn recv_async(&self) -> RecvOutcome<FrameLease> {
        let start = Instant::now();
        match self.rx.recv_async().await {
            RecvOutcome::Data(frame) => {
                self.metrics.record(start.elapsed());
                RecvOutcome::Data(frame)
            }
            other => other,
        }
    }

    /// Wait up to `wait` for a frame.
    ///
    /// Returns `RecvOutcome::Empty` when no frame arrives before `wait` elapses.
    pub fn recv_blocking(&self, wait: std::time::Duration) -> RecvOutcome<FrameLease> {
        match self.recv_timeout(wait) {
            styx_core::queue::RecvWaitOutcome::Data(frame) => RecvOutcome::Data(frame),
            styx_core::queue::RecvWaitOutcome::Closed => RecvOutcome::Closed,
            styx_core::queue::RecvWaitOutcome::Timeout => RecvOutcome::Empty,
        }
    }

    /// Wait indefinitely until a frame arrives or the capture closes.
    pub fn recv_forever(&self) -> RecvOutcome<FrameLease> {
        let start = Instant::now();
        match self.rx.recv_blocking() {
            styx_core::queue::RecvWaitOutcome::Data(frame) => {
                self.metrics.record(start.elapsed());
                RecvOutcome::Data(frame)
            }
            styx_core::queue::RecvWaitOutcome::Closed => RecvOutcome::Closed,
            styx_core::queue::RecvWaitOutcome::Timeout => RecvOutcome::Empty,
        }
    }

    /// Receive with explicit timeout semantics.
    ///
    /// Unlike `recv_blocking`, this returns `RecvWaitOutcome::Timeout` directly
    /// instead of mapping it to `RecvOutcome::Empty`.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> styx_core::queue::RecvWaitOutcome<FrameLease> {
        let start = Instant::now();
        let outcome = self.rx.recv_timeout(timeout);
        match outcome {
            styx_core::queue::RecvWaitOutcome::Data(frame) => {
                self.metrics.record(start.elapsed());
                styx_core::queue::RecvWaitOutcome::Data(frame)
            }
            styx_core::queue::RecvWaitOutcome::Closed => styx_core::queue::RecvWaitOutcome::Closed,
            styx_core::queue::RecvWaitOutcome::Timeout => {
                styx_core::queue::RecvWaitOutcome::Timeout
            }
        }
    }

    /// Iterate frames, ignoring timeout wakeups and ending when capture closes.
    pub fn frames_blocking(&self, wait: std::time::Duration) -> CaptureFrameIter<'_> {
        CaptureFrameIter::new(self, wait)
    }

    /// Stop the capture worker.
    pub fn stop(mut self) {
        self.teardown_in_place();
    }

    /// Stop the capture worker without consuming the handle.
    pub fn stop_in_place(&mut self) {
        self.teardown_in_place();
    }

    /// Async variant of stop.
    #[cfg(feature = "async")]
    pub async fn stop_async(mut self) {
        self.teardown_async_in_place().await;
    }

    fn teardown_in_place(&mut self) {
        let teardown_started = Instant::now();
        let backend = self.backend;
        self.signal_stop_and_close();
        if let Some(worker) = self.worker.take() {
            join_worker_sync(backend, worker, "capture worker");
        }
        for worker in self.aux_workers.drain(..) {
            join_worker_sync(backend, worker, "capture auxiliary worker");
        }
        self.finish_teardown();
        tracing::debug!(
            backend = %backend,
            teardown_ms = teardown_started.elapsed().as_millis() as u64,
            "capture teardown complete"
        );
    }

    #[cfg(feature = "async")]
    async fn teardown_async_in_place(&mut self) {
        let teardown_started = Instant::now();
        let backend = self.backend;
        self.signal_stop_and_close();
        if let Some(worker) = self.worker.take() {
            join_worker_async(backend, worker, "capture worker").await;
        }
        for worker in self.aux_workers.drain(..) {
            join_worker_async(backend, worker, "capture auxiliary worker").await;
        }
        self.finish_teardown();
        tracing::debug!(
            backend = %backend,
            teardown_ms = teardown_started.elapsed().as_millis() as u64,
            "capture async teardown complete"
        );
    }

    fn signal_stop_and_close(&mut self) {
        let start = Instant::now();
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        self.rx.close();
        tracing::debug!(
            backend = %self.backend,
            signal_close_ms = start.elapsed().as_millis() as u64,
            "capture stop signaled and queue closed"
        );
    }

    fn finish_teardown(&mut self) {
        let start = Instant::now();
        let mut drained = 0u64;
        while let RecvOutcome::Data(_frame) = self.rx.recv() {
            drained = drained.saturating_add(1);
        }
        tracing::debug!(
            backend = %self.backend,
            drained_frames = drained,
            drain_ms = start.elapsed().as_millis() as u64,
            "capture queue drained during teardown"
        );
        #[cfg(feature = "libcamera")]
        if self.libcamera_idle_stop_allowed {
            libcamera_backend::stop_manager_if_idle(self.libcamera_stop_when_idle);
        }
    }

    /// Reconfigure capture by stopping this session and starting a new one from a request.
    ///
    /// This consumes the old handle and returns a fresh one.
    pub fn reconfigure(
        self,
        request: super::CaptureRequest<'_>,
    ) -> Result<CaptureHandle, CaptureError> {
        let mut handle = self;
        handle.reconfigure_in_place(request)?;
        Ok(handle)
    }

    /// Reconfigure capture using Styx-owned retry and fallback behavior.
    pub fn reconfigure_with_policy(
        self,
        request: super::CaptureRequest<'_>,
        policy: CaptureStartPolicy,
    ) -> Result<CaptureHandle, CaptureError> {
        let mut handle = self;
        handle.reconfigure_in_place_with_policy(request, policy)?;
        Ok(handle)
    }

    /// Reconfigure capture in-place by stopping the current worker before starting a new one.
    ///
    /// This fully restarts the camera, which is required when changing resolution or pixel formats.
    pub fn reconfigure_in_place(
        &mut self,
        request: super::CaptureRequest<'_>,
    ) -> Result<(), CaptureError> {
        self.teardown_in_place();
        let mut new_capture = request.start()?;
        // Swap to avoid double-drop; the torn-down handle will drop harmlessly.
        mem::swap(self, &mut new_capture);
        Ok(())
    }

    /// Reconfigure capture in-place using Styx-owned retry and fallback behavior.
    pub fn reconfigure_in_place_with_policy(
        &mut self,
        request: super::CaptureRequest<'_>,
        policy: CaptureStartPolicy,
    ) -> Result<(), CaptureError> {
        self.teardown_in_place();
        let mut new_capture = request.start_with_policy(policy)?;
        mem::swap(self, &mut new_capture);
        Ok(())
    }

    /// Backend kind used for this capture.
    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    /// Mode in use.
    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Interval in use (if advertised).
    pub fn interval(&self) -> Option<Interval> {
        self.interval
    }

    /// Capture timing metrics (per-frame wait/receive durations).
    pub fn metrics(&self) -> StageMetrics {
        self.metrics.clone()
    }

    /// Last asynchronous backend worker error observed after capture startup.
    pub fn last_error(&self) -> Option<CaptureError> {
        self.worker_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    /// Last control-plane error observed on this handle.
    pub fn last_control_error(&self) -> Option<CaptureError> {
        self.control_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    pub fn queue_stats(&self) -> crate::metrics::QueueTelemetryStats {
        self.rx.stats().into()
    }

    /// Snapshot capture/decode/transform pool usage visible to this process.
    pub fn memory_stats(&self) -> crate::metrics::PipelineMemoryStats {
        crate::metrics::PipelineMemoryStats {
            capture_queue: Some(crate::metrics::QueueMemoryStats {
                depth: self.rx.len() as u64,
                capacity: self.rx.capacity() as u64,
            }),
            external_backings: self
                .external_backings
                .iter()
                .map(|tracker| tracker.snapshot())
                .collect(),
            transform_pool: styx_core::transform::transform_pool_stats(),
        }
    }

    pub fn health_report(&self) -> crate::metrics::HealthReport {
        let queue = self.queue_stats();
        let capture = self.metrics.snapshot();
        let memory = self.memory_stats();
        let mut drop_reasons = Vec::new();
        crate::metrics::push_drop_reason(
            &mut drop_reasons,
            crate::metrics::FrameDropReason::CaptureQueueSendTimeout,
            queue.send_timeouts,
        );
        let external_inflight_buffers = memory
            .external_backings
            .iter()
            .map(|stats| stats.current_buffers)
            .sum();
        let external_inflight_bytes = memory
            .external_backings
            .iter()
            .map(|stats| stats.current_bytes)
            .sum();
        let drop_count = crate::metrics::total_frame_drops(&drop_reasons);
        let mut recent_stage_errors = Vec::new();
        if let Some(err) = self.last_error() {
            recent_stage_errors.push(crate::metrics::PipelineStageError {
                stage: crate::metrics::PipelineStage::Capture,
                component: self.backend.to_string(),
                message: err.to_string(),
            });
        }
        if let Some(err) = self.last_control_error() {
            recent_stage_errors.push(crate::metrics::PipelineStageError {
                stage: crate::metrics::PipelineStage::Capture,
                component: format!("{}.control", self.backend),
                message: err.to_string(),
            });
        }
        crate::metrics::HealthReport {
            output_fps: capture.fps,
            capture_queue_depth: queue.depth,
            capture_queue_capacity: queue.capacity,
            capture_backpressure_count: queue.send_backpressure,
            drop_count,
            capture_async_send_waits: queue.async_send_waits,
            capture_async_recv_waits: queue.async_recv_waits,
            capture_async_send_wakes: queue.async_send_wakes,
            capture_async_recv_wakes: queue.async_recv_wakes,
            capture_wait_p50_ms: capture.p50_millis,
            capture_wait_p95_ms: capture.p95_millis,
            latency_p50_ms: None,
            latency_p95_ms: None,
            source_latency_p50_ms: None,
            source_latency_p95_ms: None,
            decode_p50_ms: None,
            decode_p95_ms: None,
            encode_p50_ms: None,
            encode_p95_ms: None,
            sink_p50_ms: None,
            sink_p95_ms: None,
            copy_count: 0,
            bytes_moved: 0,
            external_inflight_buffers,
            external_inflight_bytes,
            recent_residency_transitions: Vec::new(),
            recent_stage_errors,
            drop_reasons,
            graph: None,
        }
    }

    /// Apply a control to the active backend (best-effort).
    ///
    /// # Example
    /// ```rust,no_run
    /// use styx::prelude::*;
    ///
    /// let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
    /// let handle = CaptureRequest::new(&device).start()?;
    /// let _ = handle.set_control(ControlId(0), ControlValue::None);
    /// # Ok::<(), styx::capture_api::CaptureError>(())
    /// ```
    pub fn set_control(&self, _id: ControlId, _value: ControlValue) -> Result<(), CaptureError> {
        let result = apply_control_to_plane(&self.control, _id, _value);
        self.record_control_result(&result);
        result
    }

    /// Async wrapper for set_control.
    ///
    /// This uses Tokio's blocking pool because the unified control plane may touch
    /// V4L2 ioctls or wait on bounded backend responses depending on enabled
    /// features and the active backend.
    #[cfg(feature = "async")]
    pub async fn set_control_async(
        &self,
        id: ControlId,
        value: ControlValue,
    ) -> Result<(), CaptureError> {
        let control = self.control.clone();
        let result =
            tokio::task::spawn_blocking(move || apply_control_to_plane(&control, id, value))
                .await
                .map_err(|err| {
                    CaptureError::control_apply(format!("control task failed: {err}"))
                })?;
        self.record_control_result(&result);
        result
    }

    /// Fetch a control value when supported (V4L2).
    pub fn get_control(&self, _id: ControlId) -> Result<ControlValue, CaptureError> {
        let result = read_control_from_plane(&self.control, _id);
        self.record_control_result(&result);
        result
    }

    /// Async wrapper for get_control.
    ///
    /// This uses Tokio's blocking pool because the unified control plane may touch
    /// V4L2 ioctls or wait on bounded backend responses depending on enabled
    /// features and the active backend.
    #[cfg(feature = "async")]
    pub async fn get_control_async(&self, id: ControlId) -> Result<ControlValue, CaptureError> {
        let control = self.control.clone();
        let result = tokio::task::spawn_blocking(move || read_control_from_plane(&control, id))
            .await
            .map_err(|err| CaptureError::control_apply(format!("control task failed: {err}")))?;
        self.record_control_result(&result);
        result
    }

    fn record_control_result<T>(&self, result: &Result<T, CaptureError>) {
        if let Err(err) = result
            && let Ok(mut error) = self.control_error.lock()
        {
            *error = Some(err.clone());
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        // If the consumer forgot to call stop, attempt a best-effort shutdown to avoid leaks.
        if self.worker.is_some() || !self.aux_workers.is_empty() {
            self.teardown_in_place();
        }
    }
}

fn join_worker_sync(backend: BackendKind, worker: WorkerHandle, label: &'static str) {
    let join_started = Instant::now();
    match worker {
        WorkerHandle::Thread(h) => {
            let _ = h.join();
            tracing::debug!(
                backend = %backend,
                worker = label,
                join_ms = join_started.elapsed().as_millis() as u64,
                "capture worker thread joined"
            );
        }
        #[cfg(feature = "async")]
        WorkerHandle::Async(h) => {
            h.abort();
            tracing::debug!(
                backend = %backend,
                worker = label,
                abort_ms = join_started.elapsed().as_millis() as u64,
                "capture async worker aborted during sync teardown"
            );
        }
    }
}

#[cfg(feature = "async")]
async fn join_worker_async(backend: BackendKind, worker: WorkerHandle, label: &'static str) {
    let join_started = Instant::now();
    match worker {
        WorkerHandle::Thread(h) => {
            let _ = tokio::task::spawn_blocking(move || h.join()).await;
            tracing::debug!(
                backend = %backend,
                worker = label,
                join_ms = join_started.elapsed().as_millis() as u64,
                "capture worker thread joined from async teardown"
            );
        }
        WorkerHandle::Async(h) => {
            let _ = h.await;
            tracing::debug!(
                backend = %backend,
                worker = label,
                join_ms = join_started.elapsed().as_millis() as u64,
                "capture async worker joined"
            );
        }
    }
}

impl CaptureSource for CaptureHandle {
    fn descriptor(&self) -> &CaptureDescriptor {
        &self.descriptor
    }

    fn next_frame(&self) -> Option<FrameLease> {
        match self.recv_forever() {
            RecvOutcome::Data(frame) => Some(frame),
            RecvOutcome::Closed | RecvOutcome::Empty => None,
        }
    }
}

pub(crate) fn start_backend(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    descriptor: CaptureDescriptor,
    _controls: Vec<(ControlId, ControlValue)>,
    _tdn_output_mode: TdnOutputMode,
    config: &StyxConfig,
) -> Result<CaptureHandle, CaptureError> {
    match backend.kind {
        BackendKind::Virtual => virtual_backend::start_virtual(mode, interval, descriptor, config),
        #[cfg(feature = "v4l2")]
        BackendKind::V4l2 => {
            v4l2_backend::start_v4l2(backend, mode, interval, _controls, descriptor, config)
        }
        #[cfg(not(feature = "v4l2"))]
        BackendKind::V4l2 => Err(CaptureError::BackendMissing(BackendKind::V4l2)),
        #[cfg(feature = "libcamera")]
        BackendKind::Libcamera => libcamera_backend::start_libcamera(
            backend,
            mode,
            interval,
            _controls,
            descriptor,
            _tdn_output_mode,
            config,
        ),
        #[cfg(not(feature = "libcamera"))]
        BackendKind::Libcamera => Err(CaptureError::BackendMissing(BackendKind::Libcamera)),
        #[cfg(feature = "netcam")]
        BackendKind::Netcam => {
            netcam_backend::start_netcam(backend, mode, interval, descriptor, config)
        }
        #[cfg(not(feature = "netcam"))]
        BackendKind::Netcam => Err(CaptureError::BackendMissing(BackendKind::Netcam)),
        #[cfg(feature = "file-backend")]
        BackendKind::File => {
            file_backend::start_file(backend, mode, interval, _controls, descriptor, config)
        }
        #[cfg(not(feature = "file-backend"))]
        BackendKind::File => Err(CaptureError::BackendMissing(BackendKind::File)),
        #[cfg(feature = "simulation-bevy")]
        BackendKind::Simulation => simulation_backend::start_simulation(
            backend, mode, interval, _controls, descriptor, config,
        ),
        #[cfg(not(feature = "simulation-bevy"))]
        BackendKind::Simulation => Err(CaptureError::BackendMissing(BackendKind::Simulation)),
    }
}
