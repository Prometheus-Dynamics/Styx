//! Preview encoders (`styx::preview`): frames offered, encoded and dropped by cause, encode and
//! scale times, JPEG sizes, and the CPU time of the preview thread.

// Only the `preview` feature makes previews; the snapshot type is always there.
#![cfg_attr(not(all(feature = "preview", target_os = "linux")), allow(dead_code))]

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};

use styx_core::metrics::Ring;

use super::camera::Window;
use super::live::RingWindow;
use super::openmetrics::{Text, esc};

static PREVIEWS: Mutex<Vec<Weak<PreviewCounters>>> = Mutex::new(Vec::new());

/// A preview's counters, written by its thread with relaxed atomics.
#[derive(Default)]
pub(crate) struct PreviewCounters {
    pub(crate) name: String,
    pub(crate) encoder: String,
    pub(crate) started: std::sync::OnceLock<std::time::Instant>,
    pub(crate) frames_in: AtomicU64,
    pub(crate) encoded: AtomicU64,
    pub(crate) dropped_rate: AtomicU64,
    pub(crate) dropped_busy: AtomicU64,
    pub(crate) dropped_unwatched: AtomicU64,
    pub(crate) errors: AtomicU64,
    pub(crate) bytes: AtomicU64,
    pub(crate) passthrough: AtomicU64,
    pub(crate) width: AtomicU64,
    pub(crate) height: AtomicU64,
    pub(crate) source_width: AtomicU64,
    pub(crate) source_height: AtomicU64,
    pub(crate) cpu_ns: AtomicU64,
    pub(crate) subscribers: AtomicU64,
    pub(crate) encode: Ring,
    pub(crate) scale: Ring,
    /// Bytes per JPEG (as "nanoseconds" in the ring; read back as bytes).
    pub(crate) jpeg_bytes: Ring,
    /// Capture to encoded, for frames whose timestamp is on a system clock.
    pub(crate) latency: Ring,
    pub(crate) last_error: Mutex<Option<String>>,
}

impl PreviewCounters {
    pub(crate) fn new(name: String, encoder: String) -> Arc<Self> {
        let counters = Arc::new(Self {
            name,
            encoder,
            ..Default::default()
        });
        let _ = counters.started.set(std::time::Instant::now());
        if let Ok(mut all) = PREVIEWS.lock() {
            all.retain(|w| w.strong_count() > 0);
            all.push(Arc::downgrade(&counters));
        }
        counters
    }

    pub(crate) fn error(&self, err: String) {
        self.errors.fetch_add(1, Relaxed);
        if let Ok(mut last) = self.last_error.lock() {
            *last = Some(err);
        }
    }

    pub(crate) fn snapshot(&self) -> PreviewMetrics {
        let bytes = self.jpeg_bytes.samples();
        let (mut sorted, n) = (bytes.clone(), bytes.len().max(1) as u64);
        sorted.sort_unstable();
        let uptime = self
            .started
            .get()
            .map_or(0.0, |t| t.elapsed().as_secs_f64());
        let cpu_ns = self.cpu_ns.load(Relaxed);
        PreviewMetrics {
            name: self.name.clone(),
            encoder: self.encoder.clone(),
            frames_in: self.frames_in.load(Relaxed),
            encoded: self.encoded.load(Relaxed),
            passthrough: self.passthrough.load(Relaxed),
            dropped_rate: self.dropped_rate.load(Relaxed),
            dropped_busy: self.dropped_busy.load(Relaxed),
            dropped_unwatched: self.dropped_unwatched.load(Relaxed),
            errors: self.errors.load(Relaxed),
            bytes_total: self.bytes.load(Relaxed),
            bytes_per_frame: (!bytes.is_empty()).then(|| bytes.iter().sum::<u64>() / n),
            bytes_p95: sorted
                .get(((sorted.len().saturating_sub(1)) as f64 * 0.95).round() as usize)
                .copied(),
            size: (
                self.width.load(Relaxed) as u32,
                self.height.load(Relaxed) as u32,
            ),
            source_size: (
                self.source_width.load(Relaxed) as u32,
                self.source_height.load(Relaxed) as u32,
            ),
            encode: self.encode.window(),
            scale: self.scale.window(),
            capture_to_encoded: self.latency.window(),
            cpu_ns,
            cpu_percent: (uptime > 0.0).then(|| cpu_ns as f64 / 1e9 / uptime * 100.0),
            subscribers: self.subscribers.load(Relaxed),
            last_error: self.last_error.lock().ok().and_then(|e| e.clone()),
        }
    }
}

/// A preview encoder's metrics (`styx::preview::Preview::metrics`, and in
/// [`MetricsSnapshot::previews`](super::MetricsSnapshot::previews)).
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct PreviewMetrics {
    /// The preview's name (`PreviewConfig::name`).
    pub name: String,
    /// The JPEG encoder (`turbojpeg`, `mozjpeg`, `image`).
    pub encoder: String,
    /// Frames offered to the preview (or received from the camera service).
    pub frames_in: u64,
    /// JPEG frames published (encoded, or passed through).
    pub encoded: u64,
    /// Of those, camera JPEG (MJPEG) frames passed through without re-encoding.
    pub passthrough: u64,
    /// Frames dropped by the frame rate cap.
    pub dropped_rate: u64,
    /// Frames replaced by a newer one while the encoder was busy (latest-frame semantics).
    pub dropped_busy: u64,
    /// Frames dropped because nobody was watching.
    pub dropped_unwatched: u64,
    /// Frames that could not be scaled or encoded (`last_error` says why).
    pub errors: u64,
    /// JPEG bytes published.
    pub bytes_total: u64,
    /// Mean JPEG size over the last [`WINDOW`](super::WINDOW) frames.
    pub bytes_per_frame: Option<u64>,
    pub bytes_p95: Option<u64>,
    /// The preview's size and the size of the frames it was scaled from (latest).
    pub size: (u32, u32),
    pub source_size: (u32, u32),
    /// JPEG encode time per frame.
    pub encode: Window,
    /// Source selection and scaling to the preview size per frame (with the frame held).
    pub scale: Window,
    /// Capture timestamp to JPEG published (frames on a system clock).
    pub capture_to_encoded: Window,
    /// CPU time of the preview thread since it started (`CLOCK_THREAD_CPUTIME_ID`).
    pub cpu_ns: u64,
    /// That as a share of one core since the preview started.
    pub cpu_percent: Option<f64>,
    /// Subscribers now.
    pub subscribers: u64,
    pub last_error: Option<String>,
}

/// Every preview running in the process.
pub(crate) fn previews() -> Vec<PreviewMetrics> {
    PREVIEWS
        .lock()
        .map(|all| {
            all.iter()
                .filter_map(Weak::upgrade)
                .map(|p| p.snapshot())
                .collect()
        })
        .unwrap_or_default()
}

impl Text {
    pub(super) fn preview(&mut self, p: &PreviewMetrics) {
        let l = format!(
            "preview=\"{}\",encoder=\"{}\"",
            esc(&p.name),
            esc(&p.encoder)
        );
        self.counter(
            "styx_preview_frames_in_total",
            "Frames offered to the preview.",
            &l,
            p.frames_in,
        );
        self.counter(
            "styx_preview_frames_encoded_total",
            "JPEG frames published (encoded or passed through).",
            &l,
            p.encoded,
        );
        for (cause, v) in [
            ("rate", p.dropped_rate),
            ("busy", p.dropped_busy),
            ("unwatched", p.dropped_unwatched),
            ("error", p.errors),
        ] {
            self.counter(
                "styx_preview_frames_dropped_total",
                "Frames the preview did not publish, by cause.",
                &format!("{l},cause=\"{cause}\""),
                v,
            );
        }
        self.counter(
            "styx_preview_bytes_total",
            "JPEG bytes published.",
            &l,
            p.bytes_total,
        );
        self.gauge(
            "styx_preview_bytes_per_frame",
            "Mean JPEG size over the window.",
            &l,
            p.bytes_per_frame.map(|b| b as f64),
        );
        self.window(
            "styx_preview_encode_ms",
            "JPEG encode time per frame.",
            &l,
            &p.encode,
        );
        self.window(
            "styx_preview_scale_ms",
            "Scaling to the preview size per frame.",
            &l,
            &p.scale,
        );
        self.window(
            "styx_preview_capture_to_encoded_ms",
            "Capture timestamp to JPEG published.",
            &l,
            &p.capture_to_encoded,
        );
        self.sample(
            "styx_preview_cpu_seconds_total",
            "counter",
            "CPU time of the preview thread.",
            &l,
            p.cpu_ns as f64 / 1e9,
        );
        self.gauge(
            "styx_preview_subscribers",
            "Viewers subscribed to the preview.",
            &l,
            Some(p.subscribers as f64),
        );
    }
}
