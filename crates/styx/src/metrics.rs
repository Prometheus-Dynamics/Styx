//! Pipeline health reports, and per-camera metrics cheap enough to leave on: see
//! [`snapshot`], [`CaptureHandle::camera_metrics`](crate::capture_api::CaptureHandle::camera_metrics)
//! and `docs/metrics.md`.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[path = "metrics/af.rs"]
mod af;
#[path = "metrics/camera.rs"]
mod camera;
#[path = "metrics/export.rs"]
mod export;
#[path = "metrics/live.rs"]
mod live;
#[path = "metrics/openmetrics.rs"]
mod openmetrics;
#[path = "metrics/path.rs"]
mod path;
#[path = "metrics/preview.rs"]
mod preview;
#[path = "metrics/registry.rs"]
mod registry;
#[path = "metrics/retry.rs"]
mod retry;
#[cfg(any(feature = "libcamera", feature = "v4l2"))]
#[path = "metrics/sequence_gaps.rs"]
mod sequence_gaps;

#[cfg(feature = "native")]
pub(crate) use af::{AfModeKind, AfSample, AfStateKind};
pub(crate) use camera::Attached;
pub use camera::{
    AaaState, Buffers, CameraMetrics, ConsumerMetrics, Cpu, Drops, FrameCounts, FrameRate,
    IspTimes, Latency, Restarts, Window,
};
pub use export::{MetricKind, MetricSample, render_prometheus};
#[cfg(feature = "native")]
pub(crate) use live::AaaSample;
pub(crate) use live::ConsumerStats;
#[cfg(feature = "frame-socket")]
pub(crate) use live::RingWindow;
pub(crate) use live::stamp_queued;
pub use live::{CaptureMetrics, WINDOW, frame_path_cost, hop_path_cost};
#[cfg(feature = "metrics-http")]
pub use openmetrics::{MetricsHttpServer, serve_http};
#[cfg(feature = "frame-socket")]
pub(crate) use path::frame_socket_text;
pub use path::{CopySiteMetrics, HopMetrics, HopWindow, PathMetrics, hop_lines, path};
#[cfg(all(feature = "preview", target_os = "linux"))]
pub(crate) use preview::PreviewCounters;
pub use preview::PreviewMetrics;
pub use registry::{MetricsSnapshot, ProcessMetrics, ServiceMetrics, process, snapshot};
pub(crate) use registry::{register, service_client};
pub use retry::{CaptureRetryMetrics, CaptureRetryStats};
#[cfg(any(feature = "libcamera", feature = "v4l2"))]
pub(crate) use sequence_gaps::SequenceGapTracker;

const DEFAULT_WINDOW: usize = 120;
const DEFAULT_TRANSITION_WINDOW: usize = 16;
const DEFAULT_STAGE_ERROR_WINDOW: usize = 16;

#[derive(Clone, Debug, Default)]
pub struct StageSnapshot {
    pub samples: u64,
    pub total_samples: u64,
    pub last_millis: Option<f64>,
    pub avg_millis: Option<f64>,
    pub p50_millis: Option<f64>,
    pub p95_millis: Option<f64>,
    pub fps: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct CopyStats {
    pub copies: u64,
    pub bytes_moved: u64,
    pub input_frames: u64,
    pub external_input_frames: u64,
    pub zero_copy_output_frames: u64,
}

#[derive(Clone, Default)]
pub struct CopyMetrics {
    inner: Arc<CopyState>,
}

#[derive(Default)]
struct CopyState {
    copies: AtomicU64,
    bytes_moved: AtomicU64,
    input_frames: AtomicU64,
    external_input_frames: AtomicU64,
    zero_copy_output_frames: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub struct PipelineMemoryStats {
    pub capture_queue: Option<QueueMemoryStats>,
    pub external_backings: Vec<ExternalBackingStats>,
    pub transform_pool: Option<styx_core::buffer::BufferPoolStats>,
    pub decoder_pool: Option<styx_core::buffer::BufferPoolStats>,
    pub encoder_pool: Option<styx_core::buffer::BufferPoolStats>,
    #[cfg(target_os = "linux")]
    pub shared_decode_pool: Option<styx_core::buffer::SharedBufferPoolStats>,
    #[cfg(target_os = "linux")]
    pub shared_encode_pool: Option<styx_core::buffer::SharedBufferPoolStats>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureShutdownStats {
    pub last_signal_close_ms: Option<u64>,
    pub last_worker_join_ms: Option<u64>,
    pub last_worker_wait_outcome: Option<CaptureShutdownWorkerWaitOutcome>,
    pub last_teardown_ms: Option<u64>,
    pub last_drain_ms: Option<u64>,
    pub last_drained_frames: u64,
    pub async_drop_detached_workers: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureShutdownWorkerWaitOutcome {
    Joined,
    DetachedInAsyncDrop,
}

#[derive(Clone, Debug, Default)]
pub struct HealthReport {
    pub output_fps: Option<f64>,
    pub capture_queue_depth: u64,
    pub capture_queue_capacity: u64,
    pub capture_backpressure_count: u64,
    pub drop_count: u64,
    pub capture_async_send_waits: u64,
    pub capture_async_recv_waits: u64,
    pub capture_async_send_wakes: u64,
    pub capture_async_recv_wakes: u64,
    pub capture_wait_p50_ms: Option<f64>,
    pub capture_wait_p95_ms: Option<f64>,
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
    pub source_latency_p50_ms: Option<f64>,
    pub source_latency_p95_ms: Option<f64>,
    /// Sensor timestamp → capture backend delivery (ISP/decode-in-driver/transport latency).
    pub sensor_latency_p50_ms: Option<f64>,
    pub sensor_latency_p95_ms: Option<f64>,
    pub decode_p50_ms: Option<f64>,
    pub decode_p95_ms: Option<f64>,
    pub encode_p50_ms: Option<f64>,
    pub encode_p95_ms: Option<f64>,
    pub sink_p50_ms: Option<f64>,
    pub sink_p95_ms: Option<f64>,
    pub copy_count: u64,
    pub bytes_moved: u64,
    pub external_inflight_buffers: u64,
    pub external_inflight_bytes: u64,
    pub recent_residency_transitions: Vec<styx_core::buffer::ResidencyTransition>,
    pub recent_stage_errors: Vec<PipelineStageError>,
    pub drop_reasons: Vec<FrameDropStats>,
    pub capture_shutdown: CaptureShutdownStats,
    pub capture_retries: CaptureRetryStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineStage {
    Capture,
    Graph,
    Decode,
    Encode,
    Transform,
    Sink,
}

impl std::fmt::Display for PipelineStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Capture => "capture",
            Self::Graph => "graph",
            Self::Decode => "decode",
            Self::Encode => "encode",
            Self::Transform => "transform",
            Self::Sink => "sink",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineStageError {
    pub stage: PipelineStage,
    pub component: String,
    pub message: String,
}

impl std::fmt::Display for PipelineStageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} stage {} failed: {}",
            self.stage, self.component, self.message
        )
    }
}

impl std::error::Error for PipelineStageError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameDropReason {
    CaptureQueueSendTimeout,
    /// Queued frames replaced by newer ones (`QueueOverflow::DropOldest`).
    CaptureQueueEviction,
    /// Frames missing from the backend's sequence numbers: lost by the sensor, driver or ISP
    /// before Styx received them (libcamera and V4L2).
    SensorSequenceGap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameDropStats {
    pub reason: FrameDropReason,
    pub count: u64,
}

pub(crate) fn push_drop_reason(
    reasons: &mut Vec<FrameDropStats>,
    reason: FrameDropReason,
    count: u64,
) {
    if count > 0 {
        reasons.push(FrameDropStats { reason, count });
    }
}

pub(crate) fn total_frame_drops(reasons: &[FrameDropStats]) -> u64 {
    reasons.iter().map(|stats| stats.count).sum()
}

#[derive(Clone, Debug, Default)]
pub struct ResidencySnapshot {
    pub transitions: Vec<styx_core::buffer::ResidencyTransition>,
}

#[derive(Clone, Default)]
pub struct ResidencyMetrics {
    inner: Arc<Mutex<VecDeque<styx_core::buffer::ResidencyTransition>>>,
}

#[derive(Clone, Default)]
pub struct StageErrorMetrics {
    inner: Arc<Mutex<VecDeque<PipelineStageError>>>,
}

#[derive(Clone, Debug, Default)]
pub struct QueueMemoryStats {
    pub depth: u64,
    pub capacity: u64,
}

#[derive(Clone, Debug, Default)]
pub struct QueueTelemetryStats {
    pub depth: u64,
    pub capacity: u64,
    pub send_backpressure: u64,
    pub send_timeouts: u64,
    pub evictions: u64,
    pub sent: u64,
    pub recv_empty: u64,
    pub recv_timeouts: u64,
    pub async_send_waits: u64,
    pub async_recv_waits: u64,
    pub async_send_wakes: u64,
    pub async_recv_wakes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ExternalBackingStats {
    pub label: String,
    pub current_buffers: u64,
    pub current_bytes: u64,
    pub peak_buffers: u64,
    pub peak_bytes: u64,
}

#[derive(Debug)]
pub struct ExternalBackingTracker {
    label: &'static str,
    current_buffers: AtomicU64,
    current_bytes: AtomicU64,
    peak_buffers: AtomicU64,
    peak_bytes: AtomicU64,
}

// Used by libcamera external backing metrics; retained in minimal builds so snapshots keep a stable
// shape across feature combinations.
#[cfg_attr(not(feature = "libcamera"), allow(dead_code))]
impl ExternalBackingTracker {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            current_buffers: AtomicU64::new(0),
            current_bytes: AtomicU64::new(0),
            peak_buffers: AtomicU64::new(0),
            peak_bytes: AtomicU64::new(0),
        }
    }

    pub fn acquire(&self, bytes: usize) {
        self.acquire_many(1, bytes);
    }

    pub fn acquire_many(&self, buffers: usize, bytes: usize) {
        let buffers = buffers as u64;
        let bytes = bytes as u64;
        if buffers == 0 && bytes == 0 {
            return;
        }
        let current_buffers = if buffers == 0 {
            self.current_buffers.load(Ordering::Relaxed)
        } else {
            self.current_buffers
                .fetch_add(buffers, Ordering::Relaxed)
                .saturating_add(buffers)
        };
        let current_bytes = if bytes == 0 {
            self.current_bytes.load(Ordering::Relaxed)
        } else {
            self.current_bytes
                .fetch_add(bytes, Ordering::Relaxed)
                .saturating_add(bytes)
        };
        self.peak_buffers
            .fetch_max(current_buffers, Ordering::Relaxed);
        self.peak_bytes.fetch_max(current_bytes, Ordering::Relaxed);
    }

    pub fn release(&self, bytes: usize) {
        self.release_many(1, bytes);
    }

    pub fn release_many(&self, buffers: usize, bytes: usize) {
        let buffers = buffers as u64;
        let bytes = bytes as u64;
        if buffers == 0 && bytes == 0 {
            return;
        }
        if buffers > 0 {
            self.current_buffers.fetch_sub(buffers, Ordering::Relaxed);
        }
        if bytes > 0 {
            self.current_bytes.fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> ExternalBackingStats {
        ExternalBackingStats {
            label: self.label.to_string(),
            current_buffers: self.current_buffers.load(Ordering::Relaxed),
            current_bytes: self.current_bytes.load(Ordering::Relaxed),
            peak_buffers: self.peak_buffers.load(Ordering::Relaxed),
            peak_bytes: self.peak_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Rolling timing metrics for a pipeline stage.
///
/// # Example
/// ```rust
/// use styx::prelude::StageMetrics;
///
/// let metrics = StageMetrics::default();
/// metrics.record(std::time::Duration::from_millis(5));
/// assert!(metrics.total_samples() >= 1);
/// ```
#[derive(Default, Clone)]
pub struct StageMetrics {
    inner: Arc<StageState>,
}

#[derive(Default)]
struct StageState {
    count: AtomicU64,
    last_nanos: AtomicU64,
    window: Mutex<WindowState>,
}

struct WindowState {
    samples: VecDeque<(Instant, u64)>,
    max: usize,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            max: DEFAULT_WINDOW,
        }
    }
}

impl StageMetrics {
    /// Record a single duration sample.
    pub fn record(&self, dur: Duration) {
        let nanos = dur.as_nanos().min(u64::MAX as u128) as u64;
        self.inner.count.fetch_add(1, Ordering::Relaxed);
        self.inner.last_nanos.store(nanos, Ordering::Relaxed);
        if let Ok(mut win) = self.inner.window.lock() {
            if win.max == 0 {
                win.max = DEFAULT_WINDOW;
            }
            win.samples.push_back((Instant::now(), nanos));
            while win.samples.len() > win.max {
                win.samples.pop_front();
            }
        }
    }

    /// Change the window size used for rolling averages/fps. Minimum of 1.
    pub fn set_window_size(&self, window: usize) {
        let window = window.max(1);
        if let Ok(mut win) = self.inner.window.lock() {
            win.max = window;
            while win.samples.len() > win.max {
                win.samples.pop_front();
            }
        }
    }

    /// Samples within the current window.
    pub fn samples(&self) -> u64 {
        self.inner
            .window
            .lock()
            .map(|w| w.samples.len() as u64)
            .unwrap_or(0)
    }

    /// Total samples recorded over the lifetime.
    pub fn total_samples(&self) -> u64 {
        self.inner.count.load(Ordering::Relaxed)
    }

    /// Rolling average of samples in milliseconds.
    pub fn avg_millis(&self) -> Option<f64> {
        self.inner.window.lock().ok().and_then(|w| {
            let count = w.samples.len();
            if count == 0 {
                return None;
            }
            let total: u128 = w.samples.iter().map(|(_, n)| *n as u128).sum();
            Some(total as f64 / 1_000_000.0 / count as f64)
        })
    }

    /// Most recent sample in milliseconds.
    pub fn last_millis(&self) -> Option<f64> {
        let last = self.inner.last_nanos.load(Ordering::Relaxed);
        if last == 0 {
            None
        } else {
            Some(last as f64 / 1_000_000.0)
        }
    }

    /// Rolling FPS based on sample timestamps.
    pub fn fps(&self) -> Option<f64> {
        self.inner.window.lock().ok().and_then(|w| {
            if w.samples.len() < 2 {
                return None;
            }
            let first = w.samples.front()?.0;
            let last = w.samples.back()?.0;
            let span = last.saturating_duration_since(first).as_secs_f64();
            if span > 0.0 {
                Some(w.samples.len() as f64 / span)
            } else {
                None
            }
        })
    }

    /// Rolling p50 of samples in milliseconds.
    pub fn p50_millis(&self) -> Option<f64> {
        self.percentile_millis(0.50)
    }

    /// Rolling p95 of samples in milliseconds.
    pub fn p95_millis(&self) -> Option<f64> {
        self.percentile_millis(0.95)
    }

    /// Snapshot common stage statistics without requiring multiple lock acquisitions downstream.
    pub fn snapshot(&self) -> StageSnapshot {
        StageSnapshot {
            samples: self.samples(),
            total_samples: self.total_samples(),
            last_millis: self.last_millis(),
            avg_millis: self.avg_millis(),
            p50_millis: self.p50_millis(),
            p95_millis: self.p95_millis(),
            fps: self.fps(),
        }
    }

    fn percentile_millis(&self, quantile: f64) -> Option<f64> {
        self.inner.window.lock().ok().and_then(|w| {
            if w.samples.is_empty() {
                return None;
            }
            let mut nanos: Vec<u64> = w.samples.iter().map(|(_, n)| *n).collect();
            nanos.sort_unstable();
            let idx = ((nanos.len().saturating_sub(1)) as f64 * quantile.clamp(0.0, 1.0)).round()
                as usize;
            nanos.get(idx).map(|value| *value as f64 / 1_000_000.0)
        })
    }
}

impl CopyMetrics {
    pub fn record_input(&self, frame: &styx_core::buffer::FrameLease) {
        self.inner.input_frames.fetch_add(1, Ordering::Relaxed);
        if frame.is_external() {
            self.inner
                .external_input_frames
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_output(&self, frame: &styx_core::buffer::FrameLease) {
        if frame.is_external() {
            self.inner
                .zero_copy_output_frames
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_copy(&self, bytes: usize) {
        self.inner.copies.fetch_add(1, Ordering::Relaxed);
        self.inner
            .bytes_moved
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> CopyStats {
        CopyStats {
            copies: self.inner.copies.load(Ordering::Relaxed),
            bytes_moved: self.inner.bytes_moved.load(Ordering::Relaxed),
            input_frames: self.inner.input_frames.load(Ordering::Relaxed),
            external_input_frames: self.inner.external_input_frames.load(Ordering::Relaxed),
            zero_copy_output_frames: self.inner.zero_copy_output_frames.load(Ordering::Relaxed),
        }
    }
}

impl ResidencyMetrics {
    pub fn record(&self, transition: styx_core::buffer::ResidencyTransition) {
        if let Ok(mut transitions) = self.inner.lock() {
            transitions.push_back(transition);
            while transitions.len() > DEFAULT_TRANSITION_WINDOW {
                transitions.pop_front();
            }
        }
    }

    pub fn snapshot(&self) -> ResidencySnapshot {
        let transitions = self
            .inner
            .lock()
            .map(|items| items.iter().copied().collect())
            .unwrap_or_default();
        ResidencySnapshot { transitions }
    }
}

impl StageErrorMetrics {
    pub fn record(
        &self,
        stage: PipelineStage,
        component: impl Into<String>,
        message: impl Into<String>,
    ) {
        if let Ok(mut errors) = self.inner.lock() {
            errors.push_back(PipelineStageError {
                stage,
                component: component.into(),
                message: message.into(),
            });
            while errors.len() > DEFAULT_STAGE_ERROR_WINDOW {
                errors.pop_front();
            }
        }
    }

    pub fn snapshot(&self) -> Vec<PipelineStageError> {
        self.inner
            .lock()
            .map(|items| items.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Metrics for a full media pipeline.
#[derive(Clone, Default)]
pub struct PipelineMetrics {
    /// Capture stage timing stats.
    pub capture: StageMetrics,
    /// Decode stage timing stats.
    pub decode: StageMetrics,
    /// Encode stage timing stats.
    pub encode: StageMetrics,
    /// Sink stage timing stats.
    pub sink: StageMetrics,
    /// Total pipeline processing latency from pipeline ingress to final output.
    pub end_to_end: StageMetrics,
    /// Source-to-sink latency using capture-time instants attached by backends.
    pub source_to_sink: StageMetrics,
    /// Sensor timestamp → capture delivery, for frames whose backend reports it.
    pub sensor_to_capture: StageMetrics,
    /// Copy/materialization counters observed while the pipeline is processing frames.
    pub copies: CopyMetrics,
    /// Residency transitions observed while the pipeline is processing frames.
    pub residency: ResidencyMetrics,
    /// Recent stage failures observed while the pipeline is processing frames.
    pub stage_errors: StageErrorMetrics,
    /// Codec registry stats.
    pub codec: styx_codec::CodecStats,
}

impl From<styx_core::queue::QueueStats> for QueueTelemetryStats {
    fn from(value: styx_core::queue::QueueStats) -> Self {
        Self {
            depth: value.depth,
            capacity: value.capacity,
            send_backpressure: value.send_backpressure,
            send_timeouts: value.send_timeouts,
            evictions: value.evictions,
            sent: value.sent,
            recv_empty: value.recv_empty,
            recv_timeouts: value.recv_timeouts,
            async_send_waits: value.async_send_waits,
            async_recv_waits: value.async_recv_waits,
            async_send_wakes: value.async_send_wakes,
            async_recv_wakes: value.async_recv_wakes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CopyMetrics, FrameDropReason, FrameDropStats, PipelineStage, ResidencyMetrics,
        StageErrorMetrics, StageMetrics, total_frame_drops,
    };
    use std::time::Duration;
    use styx_core::prelude::{
        ColorSpace, FourCc, FrameLease, FrameMeta, FrameResidency, MediaFormat,
        ResidencyTransition, ResidencyTransitionReason, Resolution, plane_layout_from_dims,
    };

    #[test]
    fn stage_metrics_snapshot_reports_percentiles() {
        let metrics = StageMetrics::default();
        for millis in [1_u64, 2, 3, 4, 10] {
            metrics.record(Duration::from_millis(millis));
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.samples, 5);
        assert_eq!(snapshot.total_samples, 5);
        assert!(snapshot.avg_millis.unwrap_or_default() >= 4.0);
        assert_eq!(snapshot.p50_millis.map(|v| v.round() as u64), Some(3));
        assert_eq!(snapshot.p95_millis.map(|v| v.round() as u64), Some(10));
    }

    #[test]
    fn copy_metrics_track_input_copy_and_zero_copy_output() {
        let metrics = CopyMetrics::default();
        let res = Resolution::new(2, 2).expect("resolution");
        let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let frame = FrameLease::from_external(
            FrameMeta::new(format, 7),
            smallvec::smallvec![plane_layout_from_dims(res.width, res.height, 3)],
            std::sync::Arc::new(TestBacking(vec![0; 12])),
        );

        metrics.record_input(&frame);
        metrics.record_copy(frame.payload_bytes());
        metrics.record_output(&frame);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.input_frames, 1);
        assert_eq!(snapshot.external_input_frames, 1);
        assert_eq!(snapshot.zero_copy_output_frames, 1);
        assert_eq!(snapshot.copies, 1);
        assert_eq!(snapshot.bytes_moved, 12);
    }

    #[test]
    fn residency_metrics_keep_recent_transitions() {
        let metrics = ResidencyMetrics::default();
        metrics.record(ResidencyTransition {
            from: FrameResidency::HostExternal,
            to: FrameResidency::HostOwned,
            reason: ResidencyTransitionReason::ImageMaterialize,
            copied: true,
        });

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.transitions.len(), 1);
        assert_eq!(
            snapshot.transitions[0].reason,
            ResidencyTransitionReason::ImageMaterialize
        );
    }

    #[test]
    fn stage_error_metrics_keep_recent_errors() {
        let metrics = StageErrorMetrics::default();
        metrics.record(PipelineStage::Decode, "mjpeg:test", "decode failed");

        let errors = metrics.snapshot();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].stage, PipelineStage::Decode);
        assert_eq!(errors[0].component, "mjpeg:test");
        assert_eq!(errors[0].message, "decode failed");
    }

    #[test]
    fn total_frame_drops_sums_all_reported_reasons() {
        let reasons = [
            FrameDropStats {
                reason: FrameDropReason::CaptureQueueSendTimeout,
                count: 2,
            },
            FrameDropStats {
                reason: FrameDropReason::CaptureQueueEviction,
                count: 3,
            },
            FrameDropStats {
                reason: FrameDropReason::SensorSequenceGap,
                count: 5,
            },
        ];

        assert_eq!(total_frame_drops(&reasons), 10);
    }

    struct TestBacking(Vec<u8>);

    impl styx_core::buffer::ExternalBacking for TestBacking {
        fn plane_data(&self, index: usize) -> Option<&[u8]> {
            (index == 0).then_some(self.0.as_slice())
        }

        fn backing_kind(&self) -> &'static str {
            "test"
        }
    }
}
