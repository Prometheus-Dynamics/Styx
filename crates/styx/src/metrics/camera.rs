//! Snapshots of capture metrics: plain data, serialisable with the `metrics-serde` feature.
//! See `docs/metrics.md` for what each value means and how it is measured.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use styx_core::prelude::FrameLease;
use styx_core::queue::BoundedRx;

use super::live::CaptureMetrics;
use super::{CaptureRetryMetrics, ExternalBackingTracker};
use crate::capture_api::CaptureError;

/// Percentiles of the last [`WINDOW`](super::WINDOW) samples of a duration, in milliseconds.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct Window {
    /// Samples in the window.
    pub samples: u64,
    /// Samples recorded since the capture started.
    pub total: u64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    /// Largest in the window.
    pub max_ms: Option<f64>,
    /// Largest since the capture started.
    pub max_ever_ms: Option<f64>,
}

impl Window {
    pub(crate) fn from_samples(mut ns: Vec<u64>, total: u64, max_ever: u64) -> Self {
        ns.sort_unstable();
        let ms = |v: u64| v as f64 / 1e6;
        let at = |q: f64| {
            let i = ((ns.len().saturating_sub(1)) as f64 * q).round() as usize;
            ns.get(i).copied().map(ms)
        };
        Self {
            samples: ns.len() as u64,
            total,
            p50_ms: at(0.5),
            p95_ms: at(0.95),
            max_ms: ns.last().copied().map(ms),
            max_ever_ms: (total > 0).then(|| ms(max_ever)),
        }
    }
}

/// Frames counted along the way from the sensor to the consumer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct FrameCounts {
    /// Frames the backend produced (before the consumer queue).
    pub captured: u64,
    /// Frames that entered the consumer queue.
    pub delivered: u64,
    /// Frames the consumer took from the queue.
    pub received: u64,
}

/// Configured and measured frame rates.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct FrameRate {
    /// The rate the capture was configured for.
    pub configured: Option<f64>,
    /// From the sensor timestamps of the last frames.
    pub measured: Option<f64>,
    /// Frames captured per second since the capture started.
    pub average: Option<f64>,
}

/// Frames lost, by cause.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct Drops {
    /// Missing from the sensor's sequence numbers: lost before Styx got them.
    pub sensor_sequence_gaps: u64,
    /// The consumer queue was full (send timed out) or a queued frame was replaced by a newer one:
    /// the consumer was too slow.
    pub queue_overflow: u64,
    /// The receiver flagged the frame as corrupt, or the backend dropped it as damaged.
    pub corrupted: u64,
    /// The ISP had no output buffer (consumers held them all).
    pub isp_skipped: u64,
    pub total: u64,
}

/// Time from a frame's sensor timestamp to stages of its delivery.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct Latency {
    /// Sensor timestamp to the frame entering the consumer queue.
    pub sensor_to_delivery: Window,
    /// Sensor timestamp to the consumer receiving the frame.
    pub sensor_to_receive: Window,
}

/// Time the ISP spent per frame.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct IspTimes {
    /// `pisp`, `software` or `gpu`.
    pub kind: String,
    /// The PiSP back end job, or the software/GPU ISP pass.
    pub isp: Window,
    /// Raw frame dequeued to outputs ready (statistics, algorithms, ISP).
    pub processing: Window,
}

/// CPU time of the capture's worker threads.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct Cpu {
    pub threads: u64,
    /// Since the capture started, in nanoseconds.
    pub total_ns: u64,
    /// `total_ns` per captured frame, in microseconds.
    pub per_frame_us: Option<f64>,
}

/// The sensor's and the 3A loop's state for the latest frame.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct AaaState {
    /// `searching` or `converged`.
    pub ae_state: Option<String>,
    pub exposure_us: Option<f64>,
    pub analogue_gain: Option<f64>,
    pub digital_gain: Option<f64>,
    pub colour_temperature_k: Option<f64>,
    pub lux: Option<f64>,
    pub awb_converged: Option<bool>,
    /// Light flicker AE detected, in Hz (100 for 50 Hz mains).
    pub flicker_hz: Option<f64>,
    pub af_state: Option<String>,
}

/// Restarts and errors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct Restarts {
    pub start_retries: u64,
    pub reconnect_attempts: u64,
    pub reconnects: u64,
    pub idle_stops: u64,
    pub last_error: Option<String>,
}

/// Capture buffers and the consumer queue.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct Buffers {
    pub queue_depth: u64,
    pub queue_capacity: u64,
    /// Capture buffers held by frames now (queued or with consumers).
    pub held: u64,
    pub held_bytes: u64,
    pub peak_held: u64,
    /// Delivery to release of each buffer.
    pub hold: Window,
}

/// One consumer of a shared capture, or one client of the camera service.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct ConsumerMetrics {
    pub label: String,
    pub received: u64,
    /// Frames this consumer did not get while others did.
    pub dropped: u64,
    /// Frames it holds now (camera service clients).
    pub held: u64,
    /// Send to release (camera service clients).
    pub hold: Window,
}

/// Everything measured for one capture.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct CameraMetrics {
    /// Process-unique id of the capture.
    pub id: u64,
    pub name: String,
    pub backend: String,
    pub mode: String,
    pub uptime_ms: u64,
    pub frames: FrameCounts,
    pub fps: FrameRate,
    pub drops: Drops,
    pub latency: Latency,
    pub isp: Option<IspTimes>,
    pub cpu: Cpu,
    #[cfg_attr(feature = "metrics-serde", serde(rename = "3a"))]
    pub aaa: Option<AaaState>,
    pub restarts: Restarts,
    pub buffers: Buffers,
    pub consumers: Vec<ConsumerMetrics>,
}

/// What a capture handle shares with its metrics for snapshots taken without it.
pub(crate) struct Attached {
    pub(crate) queue: BoundedRx<FrameLease>,
    pub(crate) worker_error: Arc<parking_lot::Mutex<Option<CaptureError>>>,
    pub(crate) retry: CaptureRetryMetrics,
    pub(crate) external: Vec<Arc<ExternalBackingTracker>>,
}

impl CaptureMetrics {
    /// A snapshot of everything measured so far.
    pub fn snapshot(&self) -> CameraMetrics {
        let l = &*self.0;
        let current = l.current.lock().ok().and_then(|c| c.clone());
        // Where the backend records frames: the running backend capture of a reconnecting one.
        let producer = current.as_ref().unwrap_or(self);
        let both = |f: &dyn Fn(&CaptureMetrics) -> u64| f(self) + current.as_ref().map_or(0, f);
        let info = l.info.lock().map(|i| i.clone()).unwrap_or_default();
        let isp_kind = info
            .isp
            .or_else(|| current.as_ref().and_then(|c| c.0.info.lock().ok()?.isp));
        let captured = both(&|m| m.0.counters.frames.load(Relaxed));
        let received = l.counters.received.load(Relaxed);
        let (queue, retry, last_error, external) = match l.attached.get() {
            Some(a) => (
                a.queue.stats(),
                a.retry.snapshot(),
                a.worker_error.lock().as_ref().map(ToString::to_string),
                a.external.iter().map(|t| t.snapshot()).collect::<Vec<_>>(),
            ),
            None => Default::default(),
        };
        let uptime = l.started.elapsed();
        let gaps = l.counters.sequence_gaps.load(Relaxed)
            + current
                .as_ref()
                .filter(|c| !Arc::ptr_eq(&c.0.counters.sequence_gaps, &l.counters.sequence_gaps))
                .map_or(0, |c| c.0.counters.sequence_gaps.load(Relaxed));
        let drops = Drops {
            sensor_sequence_gaps: gaps,
            queue_overflow: queue.send_timeouts + queue.evictions,
            corrupted: both(&|m| m.0.counters.corrupted.load(Relaxed)),
            isp_skipped: both(&|m| m.0.counters.isp_skipped.load(Relaxed)),
            total: 0,
        };
        let drops = Drops {
            total: drops.sensor_sequence_gaps
                + drops.queue_overflow
                + drops.corrupted
                + drops.isp_skipped,
            ..drops
        };
        let intervals = if producer.0.intervals.count() > 0 {
            producer.0.intervals.samples()
        } else {
            l.intervals.samples()
        };
        let measured = (!intervals.is_empty()).then(|| {
            let mean = intervals.iter().sum::<u64>() as f64 / intervals.len() as f64;
            1e9 / mean.max(1.0)
        });
        let (cpu_ns, threads) = {
            let (a, ta) = self.cpu_ns();
            let (b, tb) = current.as_ref().map_or((0, 0), |c| c.cpu_ns());
            (a + b, ta + tb)
        };
        let buffers = &producer.0.buffers;
        let ext_held: u64 = external.iter().map(|e| e.current_buffers).sum();
        let ext_bytes: u64 = external.iter().map(|e| e.current_bytes).sum();
        let ext_peak: u64 = external.iter().map(|e| e.peak_buffers).sum();
        let mut consumers: Vec<ConsumerMetrics> = l
            .consumers
            .lock()
            .map(|c| c.iter().filter_map(|w| w.upgrade()).collect::<Vec<_>>())
            .unwrap_or_default()
            .iter()
            .map(|c| c.snapshot())
            .collect();
        consumers.sort_by(|a, b| a.label.cmp(&b.label));
        CameraMetrics {
            id: l.id,
            name: info.name,
            backend: info.backend,
            mode: info.mode,
            uptime_ms: uptime.as_millis() as u64,
            frames: FrameCounts {
                captured: if producer.has_producer() || current.is_some() {
                    captured
                } else {
                    received
                },
                delivered: queue.sent,
                received,
            },
            fps: FrameRate {
                configured: info.configured_fps,
                measured,
                average: (captured > 0 && !uptime.is_zero())
                    .then(|| captured as f64 / uptime.as_secs_f64()),
            },
            drops,
            latency: Latency {
                sensor_to_delivery: producer.0.delivery.window(),
                sensor_to_receive: l.receive.window(),
            },
            isp: isp_kind.map(|kind| IspTimes {
                kind: kind.to_string(),
                isp: producer.0.isp.window(),
                processing: producer.0.processing.window(),
            }),
            cpu: Cpu {
                threads: threads as u64,
                total_ns: cpu_ns,
                per_frame_us: (captured > 0 && threads > 0)
                    .then(|| cpu_ns as f64 / 1e3 / captured as f64),
            },
            aaa: producer.aaa_state(),
            restarts: Restarts {
                start_retries: retry.start_retry_count,
                reconnect_attempts: retry.reconnect_attempts,
                reconnects: retry.reconnects,
                idle_stops: retry.idle_stops,
                last_error: last_error.or(retry.last_retry_error),
            },
            buffers: Buffers {
                queue_depth: queue.depth,
                queue_capacity: queue.capacity,
                held: buffers.held.load(Relaxed).max(0) as u64 + ext_held,
                held_bytes: buffers.held_bytes.load(Relaxed).max(0) as u64 + ext_bytes,
                peak_held: buffers.peak_held.load(Relaxed).max(0) as u64 + ext_peak,
                hold: buffers.hold.window(),
            },
            consumers,
        }
    }
}
