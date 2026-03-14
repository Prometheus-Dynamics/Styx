use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const DEFAULT_WINDOW: usize = 120;

#[derive(Clone, Debug, Default)]
pub struct PipelineMemoryStats {
    pub capture_queue: Option<QueueMemoryStats>,
    pub external_backings: Vec<ExternalBackingStats>,
    pub transform_pool: Option<styx_core::buffer::BufferPoolStats>,
    #[cfg(feature = "hooks")]
    pub image_pool: Option<styx_core::buffer::BufferPoolStats>,
    #[cfg(feature = "hooks")]
    pub packed_pools: Vec<styx_codec::decoder::PackedFramePoolStats>,
    #[cfg(feature = "hooks")]
    pub staging_copy: Option<StagingCopyStats>,
}

#[derive(Clone, Debug, Default)]
pub struct QueueMemoryStats {
    pub depth: u64,
    pub capacity: u64,
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
        let bytes = bytes as u64;
        let buffers = self
            .current_buffers
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let current_bytes = self
            .current_bytes
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        self.peak_buffers.fetch_max(buffers, Ordering::Relaxed);
        self.peak_bytes.fetch_max(current_bytes, Ordering::Relaxed);
    }

    pub fn release(&self, bytes: usize) {
        let bytes = bytes as u64;
        self.current_buffers.fetch_sub(1, Ordering::Relaxed);
        self.current_bytes.fetch_sub(bytes, Ordering::Relaxed);
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

#[cfg(feature = "hooks")]
#[derive(Clone, Debug, Default)]
pub struct StagingCopyStats {
    pub copies: u64,
    pub bytes: u64,
    pub peak_copy_bytes: u64,
}

#[cfg(feature = "hooks")]
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
    /// Codec registry stats.
    pub codec: styx_codec::CodecStats,
}
