//! Disconnect and stall recovery for libcamera and V4L2 captures.
//!
//! The consumer's handle owns a queue that outlives any one backend capture. A supervisor thread
//! runs the backend capture (feeding that queue), watches for disconnects and stalls, and
//! restarts it on the same camera, found again by identity keys.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_capture::prelude::*;

use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle};
use super::request::{CaptureError, CaptureRequest, TdnOutputMode};
use super::tunables::{ReconnectPolicy, StyxConfig};
use crate::metrics::StageMetrics;
use crate::{BackendKind, DeviceIdentity, ProbedDevice};

const POLL: Duration = Duration::from_millis(100);

/// State shared between a reconnecting capture handle and its supervisor thread (see
/// [`ReconnectPolicy`]). Opaque.
pub struct SupervisedCapture {
    /// The running backend capture; `None` while reconnecting.
    pub(crate) inner: Mutex<Option<CaptureHandle>>,
    /// Controls to apply on every (re)start: those of the request plus any set since.
    pub(crate) controls: Mutex<Vec<(ControlId, ControlValue)>>,
    /// Sequence gaps counted by backend captures that have since been replaced.
    pub(crate) past_sequence_gaps: AtomicU64,
}

impl std::fmt::Debug for SupervisedCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisedCapture")
            .field("connected", &self.inner.lock().is_some())
            .finish_non_exhaustive()
    }
}

impl SupervisedCapture {
    pub(crate) fn remember_control(&self, id: ControlId, value: ControlValue) {
        let mut controls = self.controls.lock();
        controls.retain(|(existing, _)| *existing != id);
        if !matches!(value, ControlValue::None) {
            controls.push((id, value));
        }
    }

    /// Control plane of the running capture.
    pub(crate) fn current_control(&self) -> Result<ControlPlane, CaptureError> {
        self.inner
            .lock()
            .as_ref()
            .map(|inner| inner.control.clone())
            .ok_or_else(|| CaptureError::Disconnected("camera is reconnecting".into()))
    }
}

/// What is needed to start the same capture again.
struct Recipe {
    device: ProbedDevice,
    identity: DeviceIdentity,
    backend: BackendKind,
    mode: ModeId,
    interval: Option<Interval>,
    tdn_output_mode: TdnOutputMode,
    config: StyxConfig,
}

/// The shared queue for a capture that should be supervised, or `None` when reconnection is
/// disabled or does not apply to the backend.
pub(crate) fn queue_for(backend: BackendKind, config: &StyxConfig) -> Option<CaptureQueue> {
    let capture = config.capture_tunables();
    // Virtual sources never disconnect; tests supervise them to exercise recovery.
    let supported = matches!(backend, BackendKind::Libcamera | BackendKind::V4l2)
        || (cfg!(test) && backend == BackendKind::Virtual);
    (supported && capture.reconnect.enabled)
        .then(|| styx_core::queue::bounded_with(capture.queue_depth.max(1), capture.queue_overflow))
}

/// Wrap a freshly started backend capture (started with `queue`) in a supervised handle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn supervise(
    first: CaptureHandle,
    queue: CaptureQueue,
    device: &ProbedDevice,
    mode: ModeId,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    tdn_output_mode: TdnOutputMode,
    config: StyxConfig,
) -> CaptureHandle {
    let recipe = Recipe {
        device: device.clone(),
        identity: device.identity.clone(),
        backend: first.backend,
        mode,
        interval,
        tdn_output_mode,
        config,
    };
    let backend = first.backend;
    let descriptor = first.descriptor.clone();
    let active_mode = first.mode.clone();
    let active_interval = first.interval;
    let retry_metrics = first.retry_metrics.clone();
    let shared = Arc::new(SupervisedCapture {
        inner: Mutex::new(Some(first)),
        controls: Mutex::new(controls),
        past_sequence_gaps: AtomicU64::new(0),
    });
    let worker_error = Arc::new(Mutex::new(None));
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let (tx, rx) = queue;
    let worker = {
        let shared = shared.clone();
        let worker_error = worker_error.clone();
        let retry_metrics = retry_metrics.clone();
        std::thread::Builder::new()
            .name("styx-capture-supervisor".into())
            .spawn(move || {
                run(
                    &recipe,
                    &shared,
                    &tx,
                    &stop_rx,
                    &worker_error,
                    &retry_metrics,
                );
            })
            .expect("spawn capture supervisor")
    };
    CaptureHandle {
        backend,
        control: ControlPlane::Supervised(shared),
        descriptor,
        mode: active_mode,
        interval: active_interval,
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
        retry_metrics,
        sequence_gaps: Default::default(),
    }
}

fn run(
    recipe: &Recipe,
    shared: &SupervisedCapture,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    worker_error: &Mutex<Option<CaptureError>>,
    retry_metrics: &crate::metrics::CaptureRetryMetrics,
) {
    let policy = recipe.config.capture_tunables().reconnect;
    let stall_timeout = stall_timeout(&policy, recipe.interval);
    let initial_backoff = Duration::from_millis(policy.initial_backoff_ms.max(1));
    let max_backoff = Duration::from_millis(policy.max_backoff_ms).max(initial_backoff);
    let mut backoff = initial_backoff;
    let mut next_attempt = Instant::now();
    let mut last_sent = tx.stats().sent;
    let mut last_progress = Instant::now();
    let mut restart_error: Option<String> = None;
    // Runs until stopped (or the handle, and with it the stop sender, is dropped).
    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop_rx.recv_timeout(POLL) {
        let sent = tx.stats().sent;
        if sent != last_sent {
            last_sent = sent;
            last_progress = Instant::now();
            retry_metrics.record_successful_frame();
            continue;
        }
        let Some(reason) = failure(
            shared,
            last_progress,
            stall_timeout,
            restart_error.as_deref(),
        ) else {
            continue;
        };
        if Instant::now() < next_attempt {
            continue;
        }
        tracing::warn!(backend = %recipe.backend, camera = %recipe.identity.display, reason = %reason, "capture lost; reconnecting");
        *worker_error.lock() = Some(CaptureError::Disconnected(reason.clone()));
        retry_metrics.record_disconnected_since(last_progress);
        retry_metrics.record_reconnect_attempt("reconnect", reason);
        let previous = shared.inner.lock().take();
        if let Some(previous) = previous {
            let gaps = previous.sequence_gaps.load(Ordering::Relaxed);
            shared.past_sequence_gaps.fetch_add(gaps, Ordering::Relaxed);
            previous.stop();
        }
        let controls = shared.controls.lock().clone();
        match restart(recipe, controls, backend_queue(tx)) {
            Ok(handle) => {
                tracing::info!(backend = %recipe.backend, camera = %recipe.identity.display, "capture restarted");
                *shared.inner.lock() = Some(handle);
                restart_error = None;
                backoff = initial_backoff;
                next_attempt = Instant::now();
                // Give the new capture a full stall timeout to deliver its first frame.
                last_progress = Instant::now();
            }
            Err(err) => {
                tracing::debug!(backend = %recipe.backend, error = %err, retry_in_ms = backoff.as_millis() as u64, "capture restart failed");
                restart_error = Some(format!("restart failed: {err}"));
                next_attempt = Instant::now() + backoff;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
    if let Some(inner) = shared.inner.lock().take() {
        inner.stop();
    }
}

/// Why the running capture needs a restart, if it does.
fn failure(
    shared: &SupervisedCapture,
    last_progress: Instant,
    stall: Option<Duration>,
    restart_error: Option<&str>,
) -> Option<String> {
    let guard = shared.inner.lock();
    let Some(inner) = guard.as_ref() else {
        return Some(restart_error.unwrap_or("camera not connected").to_string());
    };
    if let Some(err) = inner.last_error() {
        return Some(err.to_string());
    }
    if inner.worker_finished() {
        return Some("capture worker exited".into());
    }
    let stall = stall?;
    // Frames the consumer still holds can starve the device of buffers; that is not a stall.
    // Only lease trackers count: request pools always report their allocated buffers.
    let holding = inner
        .memory_stats()
        .external_backings
        .iter()
        .filter(|b| b.label == "v4l2_mmap" || b.label.ends_with("_lease"))
        .any(|b| b.current_buffers > 0);
    (last_progress.elapsed() >= stall && !holding)
        .then(|| format!("no frames for {} ms", last_progress.elapsed().as_millis()))
}

fn stall_timeout(policy: &ReconnectPolicy, interval: Option<Interval>) -> Option<Duration> {
    if policy.stall_timeout_ms == 0 {
        return None;
    }
    let frame = interval
        .map(|i| Duration::from_secs_f32(1.0 / i.fps().max(0.01)))
        .unwrap_or_default();
    Some(Duration::from_millis(policy.stall_timeout_ms).max(frame * 4))
}

/// Probe again and start the same capture on the camera with the most matching identity keys.
fn restart(
    recipe: &Recipe,
    controls: Vec<(ControlId, ControlValue)>,
    queue: CaptureQueue,
) -> Result<CaptureHandle, CaptureError> {
    let devices = match recipe.backend {
        BackendKind::Virtual => vec![recipe.device.clone()],
        _ => crate::probe_all(),
    };
    let device = devices
        .iter()
        .filter(|d| d.backends.iter().any(|b| b.kind == recipe.backend))
        .map(|d| {
            let shared = d
                .identity
                .keys
                .iter()
                .filter(|k| recipe.identity.keys.contains(k))
                .count();
            (shared, d)
        })
        .filter(|(shared, _)| *shared > 0)
        .max_by_key(|(shared, _)| *shared)
        .map(|(_, d)| d)
        .ok_or_else(|| {
            CaptureError::Disconnected(format!("{} not found", recipe.identity.display))
        })?;
    let mut request = CaptureRequest::new(device)
        .backend(recipe.backend)
        .mode(recipe.mode.clone())
        .tdn_output_mode(recipe.tdn_output_mode)
        .config(recipe.config.clone());
    if let Some(interval) = recipe.interval {
        request = request.interval(interval);
    }
    for (id, value) in controls {
        request = request.control(id, value);
    }
    request.start_into(queue)
}

/// The queue handed to a backend capture: the consumer's sender with a receiver of its own, so
/// stopping the backend capture never closes the consumer's queue.
pub(crate) fn backend_queue(tx: &styx_core::queue::BoundedTx<FrameLease>) -> CaptureQueue {
    (tx.clone(), styx_core::queue::bounded(1).1)
}
