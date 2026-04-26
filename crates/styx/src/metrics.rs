use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const DEFAULT_WINDOW: usize = 120;
const DEFAULT_TRANSITION_WINDOW: usize = 16;

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
    #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
    pub image_pool: Option<styx_core::buffer::BufferPoolStats>,
    #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
    pub packed_pools: Vec<styx_codec::decoder::PackedFramePoolStats>,
    #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
    pub staging_copy: Option<StagingCopyStats>,
}

#[derive(Clone, Debug, Default)]
pub struct HealthReport {
    pub output_fps: Option<f64>,
    pub capture_queue_depth: u64,
    pub capture_queue_capacity: u64,
    pub capture_backpressure_count: u64,
    pub drop_count: u64,
    pub capture_wait_p50_ms: Option<f64>,
    pub capture_wait_p95_ms: Option<f64>,
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
    pub source_latency_p50_ms: Option<f64>,
    pub source_latency_p95_ms: Option<f64>,
    pub copy_count: u64,
    pub bytes_moved: u64,
    pub external_inflight_buffers: u64,
    pub external_inflight_bytes: u64,
    pub recent_residency_transitions: Vec<styx_core::buffer::ResidencyTransition>,
}

#[derive(Clone, Debug, Default)]
pub struct ResidencySnapshot {
    pub transitions: Vec<styx_core::buffer::ResidencyTransition>,
}

#[derive(Clone, Default)]
pub struct ResidencyMetrics {
    inner: Arc<Mutex<VecDeque<styx_core::buffer::ResidencyTransition>>>,
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
    pub recv_empty: u64,
    pub recv_timeouts: u64,
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

#[cfg(all(feature = "hooks", feature = "dynamic-image"))]
#[derive(Clone, Debug, Default)]
pub struct StagingCopyStats {
    pub copies: u64,
    pub bytes: u64,
    pub peak_copy_bytes: u64,
}

#[cfg(all(feature = "hooks", feature = "dynamic-image"))]
impl From<styx_codec::decoder::StagingCopyStats> for StagingCopyStats {
    fn from(value: styx_codec::decoder::StagingCopyStats) -> Self {
        Self {
            copies: value.copies,
            bytes: value.bytes,
            peak_copy_bytes: value.peak_copy_bytes,
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

/// Metrics for a full media pipeline.
#[derive(Clone, Default)]
pub struct PipelineMetrics {
    /// Capture stage timing stats.
    pub capture: StageMetrics,
    /// Decode stage timing stats.
    pub decode: StageMetrics,
    /// Encode stage timing stats.
    pub encode: StageMetrics,
    /// Total pipeline processing latency from pipeline ingress to final output.
    pub end_to_end: StageMetrics,
    /// Source-to-sink latency using capture-time instants attached by backends.
    pub source_to_sink: StageMetrics,
    /// Copy/materialization counters observed while the pipeline is processing frames.
    pub copies: CopyMetrics,
    /// Residency transitions observed while the pipeline is processing frames.
    pub residency: ResidencyMetrics,
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
            recv_empty: value.recv_empty,
            recv_timeouts: value.recv_timeouts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CopyMetrics, ResidencyMetrics, StageMetrics};
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
        let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
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
