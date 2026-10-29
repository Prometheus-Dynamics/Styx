//! Disconnect and stall recovery, and stopping when idle, for libcamera, V4L2 and native
//! captures.
//!
//! The consumer's handle owns a queue that outlives any one backend capture. A supervisor thread
//! runs the backend capture (feeding that queue), watches for disconnects and stalls, and
//! restarts it on the same camera, found again by identity keys. With
//! `StyxConfig::stop_when_idle` it also stops the backend capture while nobody pulls frames; the
//! next pull starts it again from the consumer's thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_capture::prelude::*;

use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle};
use super::request::{CaptureError, CaptureRequest, TdnOutputMode};
use super::tunables::{IdleStop, ReconnectPolicy, StyxConfig};
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
    /// The consumer handle's metrics: counters of replaced backend captures added up, and the
    /// running one's.
    live: crate::metrics::CaptureMetrics,
    recipe: Recipe,
    tx: styx_core::queue::BoundedTx<FrameLease>,
    /// The consumer's receiver, to drop frames left over when streaming stops.
    rx: styx_core::queue::BoundedRx<FrameLease>,
    retry_metrics: crate::metrics::CaptureRetryMetrics,
    epoch: Instant,
    /// Last pull (start or end of a receive), in milliseconds since `epoch`.
    last_pull_ms: AtomicU64,
    /// Receives in progress.
    waiting: AtomicUsize,
    /// Streaming stopped because nobody pulled; `inner` is `None` meanwhile, or paused.
    idle: AtomicBool,
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

    /// The last value set for control `id`, if any.
    pub(crate) fn remembered_control(&self, id: ControlId) -> Option<ControlValue> {
        self.controls
            .lock()
            .iter()
            .find(|(existing, _)| *existing == id)
            .map(|(_, value)| value.clone())
    }

    /// Whether streaming is stopped for lack of consumers.
    pub(crate) fn is_idle(&self) -> bool {
        self.idle.load(Ordering::SeqCst)
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Record a pull for the duration of a receive; starts streaming again if it was stopped
    /// while idle.
    pub(crate) fn demand(self: &Arc<Self>) -> DemandGuard {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        self.last_pull_ms.store(self.now_ms(), Ordering::SeqCst);
        if self.is_idle() {
            self.resume();
        }
        DemandGuard(self.clone())
    }

    /// Start streaming again after an idle stop. A failed start leaves `inner` empty, which the
    /// supervisor then handles as a lost camera (with backoff).
    fn resume(&self) {
        let mut inner = self.inner.lock();
        if !self.is_idle() {
            return;
        }
        // Paused: the camera is still configured.
        if let Some(paused) = inner.as_ref() {
            if paused.resume_streaming() {
                crate::trace::debug!(camera = %self.recipe.identity.display, "capture resumed on demand");
                self.retry_metrics.record_idle_resume();
                self.idle.store(false, Ordering::SeqCst);
                return;
            }
            // The paused capture is gone: start a new one.
            if let Some(previous) = inner.take() {
                self.retire(&previous);
                previous.stop();
            }
        }
        let controls = self.controls.lock().clone();
        match restart(&self.recipe, controls, backend_queue(&self.tx)) {
            Ok(handle) => {
                crate::trace::debug!(camera = %self.recipe.identity.display, "capture resumed on demand");
                handle.attach_metrics();
                self.live.set_current(Some(handle.live.clone()));
                *inner = Some(handle);
                self.retry_metrics.record_idle_resume();
            }
            Err(err) => {
                crate::trace::warn!(camera = %self.recipe.identity.display, error = %err, "capture could not resume; reconnecting");
            }
        }
        self.idle.store(false, Ordering::SeqCst);
    }

    /// Stop streaming if nobody is receiving and the last pull is at least `after` old. Runs on
    /// the supervisor thread; the checks and the stop happen under the `inner` lock that
    /// [`SupervisedCapture::resume`] takes, so a pull cannot slip in between or restart the
    /// camera before it is released.
    fn stop_if_idle(&self, after: Duration, how: IdleStop) -> bool {
        let mut inner = self.inner.lock();
        let idle_for = self
            .now_ms()
            .saturating_sub(self.last_pull_ms.load(Ordering::SeqCst));
        if inner.is_none()
            || self.waiting.load(Ordering::SeqCst) > 0
            || idle_for < after.as_millis() as u64
        {
            return false;
        }
        self.idle.store(true, Ordering::SeqCst);
        let paused = how == IdleStop::Pause && inner.as_ref().is_some_and(|c| c.pause_streaming());
        // Frames nobody will take: release them (and their device buffers). After a pause, so
        // the first frame after resuming is a new one.
        while let RecvOutcome::Data(_) = self.rx.recv() {}
        if paused {
            drop(inner);
            self.retry_metrics.record_idle_stop();
            crate::trace::debug!(camera = %self.recipe.identity.display, idle_ms = after.as_millis() as u64, "capture paused while idle");
            return true;
        }
        if let Some(previous) = inner.take() {
            let gaps = previous.sequence_gaps.load(Ordering::Relaxed);
            self.past_sequence_gaps.fetch_add(gaps, Ordering::Relaxed);
            self.retire(&previous);
            // Still under the lock: a pull that arrives now waits for the camera to be released
            // before starting it again.
            previous.stop();
        }
        drop(inner);
        self.retry_metrics.record_idle_stop();
        crate::trace::debug!(camera = %self.recipe.identity.display, idle_ms = after.as_millis() as u64, "capture stopped while idle");
        true
    }

    /// `previous` is replaced: its counters go to the consumer handle's metrics.
    fn retire(&self, previous: &CaptureHandle) {
        self.live.absorb(&previous.live);
        self.live.set_current(None);
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

impl CaptureHandle {
    /// Stop streaming but keep the camera configured, with its buffers, for a fast
    /// [`CaptureHandle::resume_streaming`]. `false` when the backend cannot (only libcamera can).
    pub(crate) fn pause_streaming(&self) -> bool {
        match &self.control {
            #[cfg(feature = "libcamera")]
            ControlPlane::Libcamera {
                tx,
                response_timeout,
                ..
            } => {
                let (ack_tx, ack_rx) = std::sync::mpsc::channel();
                tx.send(super::libcamera_backend::ControlMessage::Pause(ack_tx))
                    .is_ok()
                    && ack_rx
                        .recv_timeout((*response_timeout).max(std::time::Duration::from_secs(2)))
                        .is_ok()
            }
            _ => false,
        }
    }

    /// Stream again after [`CaptureHandle::pause_streaming`].
    pub(crate) fn resume_streaming(&self) -> bool {
        match &self.control {
            #[cfg(feature = "libcamera")]
            ControlPlane::Libcamera { tx, .. } => tx
                .send(super::libcamera_backend::ControlMessage::Resume)
                .is_ok(),
            _ => false,
        }
    }
}

/// Marks a receive in progress; see [`SupervisedCapture::demand`].
pub(crate) struct DemandGuard(Arc<SupervisedCapture>);

impl Drop for DemandGuard {
    fn drop(&mut self) {
        let shared = &self.0;
        shared.last_pull_ms.store(shared.now_ms(), Ordering::SeqCst);
        shared.waiting.fetch_sub(1, Ordering::SeqCst);
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
    let supported = matches!(
        backend,
        BackendKind::Libcamera | BackendKind::V4l2 | BackendKind::Native | BackendKind::Uvc
    ) || (cfg!(test) && backend == BackendKind::Virtual);
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
    let live = crate::metrics::CaptureMetrics::default();
    first.attach_metrics();
    live.set_current(Some(first.live.clone()));
    let (tx, rx) = queue;
    let shared = Arc::new(SupervisedCapture {
        inner: Mutex::new(Some(first)),
        controls: Mutex::new(controls),
        past_sequence_gaps: AtomicU64::new(0),
        live: live.clone(),
        recipe,
        tx,
        rx: rx.clone(),
        retry_metrics: retry_metrics.clone(),
        epoch: Instant::now(),
        last_pull_ms: AtomicU64::new(0),
        waiting: AtomicUsize::new(0),
        idle: AtomicBool::new(false),
    });
    let worker_error = Arc::new(Mutex::new(None));
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let worker = {
        let shared = shared.clone();
        let worker_error = worker_error.clone();
        let retry_metrics = retry_metrics.clone();
        std::thread::Builder::new()
            .name("styx-capture-supervisor".into())
            .spawn(move || {
                run(&shared, &stop_rx, &worker_error, &retry_metrics);
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
        live,
    }
}

/// Retry delays of a failing capture: the first wait, doubling up to the maximum. Reset only when
/// frames flow again, so a restart that succeeds but fails a moment later (a USB stream the bus
/// refuses reports it from the capture thread) does not restart again at once.
struct Backoff {
    initial: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max: max.max(initial),
            current: initial,
        }
    }

    /// The wait before the next attempt, after one that failed or did not yet prove itself.
    fn next_wait(&mut self) -> Duration {
        let wait = self.current;
        self.current = (self.current * 2).min(self.max);
        wait
    }

    /// Frames flowed: the next failure starts the waits again from the first.
    fn progressed(&mut self) {
        self.current = self.initial;
    }
}

/// Why the running capture needs a restart.
enum Failure {
    /// No frame for the stall timeout, with the reason.
    Stalled(String),
    /// Anything else: the backend's error, the worker's exit, or a failed restart.
    Lost(String),
}

fn run(
    shared: &SupervisedCapture,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    worker_error: &Mutex<Option<CaptureError>>,
    retry_metrics: &crate::metrics::CaptureRetryMetrics,
) {
    let (recipe, tx) = (&shared.recipe, &shared.tx);
    let tunables = recipe.config.capture_tunables();
    let policy = tunables.reconnect;
    let idle_after =
        (tunables.stop_when_idle_ms > 0).then(|| Duration::from_millis(tunables.stop_when_idle_ms));
    let stall_timeout = stall_timeout(&policy, recipe.interval);
    let mut backoff = Backoff::new(
        Duration::from_millis(policy.initial_backoff_ms.max(1)),
        Duration::from_millis(policy.max_backoff_ms),
    );
    let mut next_attempt = Instant::now();
    let mut last_sent = tx.stats().sent;
    let mut last_progress = Instant::now();
    let mut restart_error: Option<String> = None;
    // A USB camera whose mode stalled or was refused by the bus is not restarted in that mode:
    // the failure is recorded against the mode, and the planner picks another one when the
    // consumers ask again.
    let mut given_up = false;
    // Runs until stopped (or the handle, and with it the stop sender, is dropped).
    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop_rx.recv_timeout(POLL) {
        // Stopped for lack of consumers: nothing to watch until a pull starts it again.
        if shared.is_idle()
            || idle_after.is_some_and(|after| shared.stop_if_idle(after, tunables.idle_stop))
        {
            last_sent = tx.stats().sent;
            last_progress = Instant::now();
            continue;
        }
        let sent = tx.stats().sent;
        if sent != last_sent {
            last_sent = sent;
            last_progress = Instant::now();
            backoff.progressed();
            retry_metrics.record_successful_frame();
            // Frames again: the disconnect is history (the retry metrics keep it, with its
            // time), not the capture's error now.
            let mut error = worker_error.lock();
            if error.is_some() {
                *error = None;
            }
            continue;
        }
        if given_up {
            continue;
        }
        let Some(failure) = failure(
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
        let usb = recipe
            .device
            .backends
            .iter()
            .find(|b| b.kind == recipe.backend)
            .and_then(crate::planner::usb_bus_mbps)
            .is_some();
        let reason = match &failure {
            Failure::Stalled(reason) | Failure::Lost(reason) => reason.clone(),
        };
        if usb && let Some(mode) = usb_mode(recipe) {
            // The mode delivered nothing, or the bus refused it: remember that for the planner,
            // stop restarting it, and report the failure to the consumers (they plan again).
            let cause = match &failure {
                Failure::Stalled(_) => {
                    crate::planner::stalled::record_stall(
                        &recipe.device,
                        recipe.backend,
                        mode,
                        &reason,
                    );
                    reason.clone()
                }
                Failure::Lost(_) if crate::planner::stalled::is_bus_full(&reason) => {
                    let _ = crate::planner::stalled::start_failed(
                        &recipe.device,
                        recipe.backend,
                        mode,
                        CaptureError::Backend(reason.clone()),
                    );
                    reason.clone()
                }
                Failure::Lost(_) => String::new(),
            };
            if !cause.is_empty() {
                crate::trace::warn!(backend = %recipe.backend, camera = %recipe.identity.display, reason = %cause, "USB capture gave up in this mode; the planner picks another");
                *worker_error.lock() = Some(CaptureError::Disconnected(cause));
                retry_metrics.record_disconnected_since(last_progress);
                if let Some(previous) = shared.inner.lock().take() {
                    shared.retire(&previous);
                    previous.stop();
                }
                given_up = true;
                continue;
            }
        }
        crate::trace::warn!(backend = %recipe.backend, camera = %recipe.identity.display, reason = %reason, "capture lost; reconnecting");
        *worker_error.lock() = Some(CaptureError::Disconnected(reason.clone()));
        retry_metrics.record_disconnected_since(last_progress);
        retry_metrics.record_reconnect_attempt("reconnect", reason);
        let previous = shared.inner.lock().take();
        if let Some(previous) = previous {
            let gaps = previous.sequence_gaps.load(Ordering::Relaxed);
            shared.past_sequence_gaps.fetch_add(gaps, Ordering::Relaxed);
            shared.retire(&previous);
            previous.stop();
        }
        let controls = shared.controls.lock().clone();
        // Whether the restart works or fails, the next attempt waits (a stream the bus refuses
        // may fail only once it runs); frames reset the wait.
        let wait = backoff.next_wait();
        next_attempt = Instant::now() + wait;
        match restart(recipe, controls, backend_queue(tx)) {
            Ok(handle) => {
                crate::trace::info!(backend = %recipe.backend, camera = %recipe.identity.display, "capture restarted");
                handle.attach_metrics();
                shared.live.set_current(Some(handle.live.clone()));
                *shared.inner.lock() = Some(handle);
                restart_error = None;
                // Give the new capture a full stall timeout to deliver its first frame.
                last_progress = Instant::now();
            }
            Err(err) => {
                crate::trace::debug!(backend = %recipe.backend, error = %err, retry_in_ms = wait.as_millis() as u64, "capture restart failed");
                restart_error = Some(format!("restart failed: {err}"));
            }
        }
    }
    if let Some(inner) = shared.inner.lock().take() {
        shared.retire(&inner);
        inner.stop();
    }
}

/// The probed mode the capture runs (its descriptor's entry for the recipe's mode id).
fn usb_mode(recipe: &Recipe) -> Option<&Mode> {
    recipe
        .device
        .backends
        .iter()
        .find(|b| b.kind == recipe.backend)?
        .descriptor
        .modes
        .iter()
        .find(|m| m.id == recipe.mode)
}

/// Why the running capture needs a restart, if it does.
fn failure(
    shared: &SupervisedCapture,
    last_progress: Instant,
    stall: Option<Duration>,
    restart_error: Option<&str>,
) -> Option<Failure> {
    let guard = shared.inner.lock();
    let Some(inner) = guard.as_ref() else {
        return Some(Failure::Lost(
            restart_error.unwrap_or("camera not connected").to_string(),
        ));
    };
    if let Some(err) = inner.last_error() {
        return Some(Failure::Lost(err.to_string()));
    }
    if inner.worker_finished() {
        return Some(Failure::Lost("capture worker exited".into()));
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
    (last_progress.elapsed() >= stall && !holding).then(|| {
        Failure::Stalled(format!(
            "no frames for {} ms",
            last_progress.elapsed().as_millis()
        ))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A restart that fails, or succeeds and fails again, waits twice as long each time up to the
    /// maximum, and frames start the waits again: no restart in a tight loop.
    #[test]
    fn restarts_that_do_not_deliver_wait_longer_each_time() {
        let ms = Duration::from_millis;
        let mut backoff = Backoff::new(ms(100), ms(1000));
        let waits: Vec<_> = (0..6).map(|_| backoff.next_wait().as_millis()).collect();
        assert_eq!(waits, [100, 200, 400, 800, 1000, 1000]);
        backoff.progressed();
        assert_eq!(backoff.next_wait(), ms(100));
    }

    #[test]
    fn native_captures_are_supervised_like_libcamera_and_v4l2_ones() {
        let config = StyxConfig::default();
        for backend in [
            BackendKind::Libcamera,
            BackendKind::V4l2,
            BackendKind::Native,
            BackendKind::Uvc,
        ] {
            assert!(
                queue_for(backend, &config).is_some()
                    == config.capture_tunables().reconnect.enabled,
                "{backend}"
            );
        }
        assert!(queue_for(BackendKind::File, &config).is_none());
    }
}
