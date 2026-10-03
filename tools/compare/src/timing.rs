//! Frame timing (rate accuracy, jitter, drops) and exposure convergence, from per-frame samples.

use serde::{Deserialize, Serialize};

/// One delivered frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameSample {
    /// Driver sequence number, when the backend reports one.
    pub sequence: Option<u32>,
    /// Frame timestamp from the backend, nanoseconds.
    pub timestamp_ns: u64,
}

/// Rate and jitter of a run of frames.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TimingStats {
    pub frames: usize,
    pub requested_fps: f64,
    /// Rate from the first and last timestamps and the frames between them (dropped frames
    /// lower it).
    pub measured_fps: f64,
    /// `(measured - requested) / requested`, percent.
    pub fps_error_percent: f64,
    /// Rate from the median frame interval: the sensor's actual rate, unaffected by drops.
    pub median_interval_fps: f64,
    pub interval_mean_us: f64,
    pub interval_std_us: f64,
    pub interval_min_us: f64,
    pub interval_max_us: f64,
    /// 99th percentile of `|interval - median interval|`.
    pub jitter_p99_us: f64,
    /// Frames missing from the sequence numbers (`None` without sequence numbers).
    pub dropped_by_sequence: Option<u64>,
    /// Intervals longer than 1.5 median intervals, counted as `round(interval / median) - 1`
    /// missing frames.
    pub dropped_by_timestamp: u64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Timing statistics of `frames` captured at `requested_fps`.
pub fn timing_stats(frames: &[FrameSample], requested_fps: f64) -> TimingStats {
    let mut stats = TimingStats {
        frames: frames.len(),
        requested_fps,
        ..TimingStats::default()
    };
    if frames.len() < 2 {
        return stats;
    }
    let intervals: Vec<f64> = frames
        .windows(2)
        .map(|w| w[1].timestamp_ns.saturating_sub(w[0].timestamp_ns) as f64 / 1e3)
        .collect();
    let n = intervals.len() as f64;
    let mean = intervals.iter().sum::<f64>() / n;
    let var = intervals.iter().map(|i| (i - mean).powi(2)).sum::<f64>() / n;
    let mut sorted = intervals.clone();
    sorted.sort_by(f64::total_cmp);
    let median = percentile(&sorted, 0.5);
    let mut dev: Vec<f64> = intervals.iter().map(|i| (i - median).abs()).collect();
    dev.sort_by(f64::total_cmp);

    let span_s = (frames[frames.len() - 1].timestamp_ns - frames[0].timestamp_ns) as f64 / 1e9;
    stats.measured_fps = if span_s > 0.0 { n / span_s } else { 0.0 };
    if requested_fps > 0.0 {
        stats.fps_error_percent = (stats.measured_fps - requested_fps) / requested_fps * 100.0;
    }
    stats.median_interval_fps = if median > 0.0 { 1e6 / median } else { 0.0 };
    stats.interval_mean_us = mean;
    stats.interval_std_us = var.sqrt();
    stats.interval_min_us = sorted[0];
    stats.interval_max_us = sorted[sorted.len() - 1];
    stats.jitter_p99_us = percentile(&dev, 0.99);
    stats.dropped_by_timestamp = intervals
        .iter()
        .filter(|&&i| median > 0.0 && i > 1.5 * median)
        .map(|&i| (i / median).round() as u64 - 1)
        .sum();
    stats.dropped_by_sequence = frames
        .windows(2)
        .map(|w| Some(u64::from(w[1].sequence?.wrapping_sub(w[0].sequence?)).saturating_sub(1)))
        .sum();
    stats
}

/// Exposure seen by one frame of the start-up window.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ExposureSample {
    /// Milliseconds since the capture was opened.
    pub at_ms: f64,
    /// Exposure time × total gain, microseconds (`None` when unknown).
    pub exposure_product_us: Option<f64>,
    /// The backend's AE state (libcamera `AeState`: 0 idle, 1 searching, 2 converged).
    pub ae_state: Option<i64>,
}

/// AE state value meaning "converged" (libcamera `AeStateConverged`).
pub const AE_STATE_CONVERGED: i64 = 2;

/// Milliseconds to the first frame the AE reports as converged.
pub fn first_converged_ms(samples: &[ExposureSample]) -> Option<f64> {
    samples
        .iter()
        .find(|s| s.ae_state == Some(AE_STATE_CONVERGED))
        .map(|s| s.at_ms)
}

/// Milliseconds to the first frame from which the exposure product stays within `tolerance`
/// (relative) of that frame's value for the next `window` frames: when the exposure stopped
/// moving, whatever moved it. `None` when it never settles within the samples.
pub fn settled_ms(samples: &[ExposureSample], window: usize, tolerance: f64) -> Option<f64> {
    let products: Vec<Option<f64>> = samples.iter().map(|s| s.exposure_product_us).collect();
    (0..samples.len()).find_map(|i| {
        let base = products[i]?;
        let end = i + window;
        if end > samples.len() || base <= 0.0 {
            return None;
        }
        products[i..end]
            .iter()
            .all(|p| p.is_some_and(|p| (p - base).abs() <= tolerance * base))
            .then_some(samples[i].at_ms)
    })
}

/// Minimum, median and maximum of a set of measurements.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Spread {
    pub min: f64,
    pub median: f64,
    pub max: f64,
    pub count: usize,
}

impl Spread {
    pub fn of(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut v = values.to_vec();
        v.sort_by(f64::total_cmp);
        Some(Self {
            min: v[0],
            median: percentile(&v, 0.5),
            max: v[v.len() - 1],
            count: v.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(times_us: &[u64], seqs: Option<&[u32]>) -> Vec<FrameSample> {
        times_us
            .iter()
            .enumerate()
            .map(|(i, &t)| FrameSample {
                sequence: seqs.map(|s| s[i]),
                timestamp_ns: t * 1_000,
            })
            .collect()
    }

    #[test]
    fn steady_rate_has_no_jitter_or_drops() {
        let t: Vec<u64> = (0..31).map(|i| i * 33_333).collect();
        let s: Vec<u32> = (0..31).collect();
        let st = timing_stats(&frames(&t, Some(&s)), 30.0);
        assert!((st.measured_fps - 30.0).abs() < 0.01, "{st:?}");
        assert!(st.interval_std_us < 1e-6);
        assert_eq!(st.dropped_by_sequence, Some(0));
        assert_eq!(st.dropped_by_timestamp, 0);
    }

    #[test]
    fn drops_show_in_sequence_and_timestamps() {
        // Frames 3 and 4 missing.
        let s = [0u32, 1, 2, 5, 6];
        let t: Vec<u64> = s.iter().map(|&i| u64::from(i) * 10_000).collect();
        let st = timing_stats(&frames(&t, Some(&s)), 100.0);
        assert_eq!(st.dropped_by_sequence, Some(2));
        assert_eq!(st.dropped_by_timestamp, 2);
        assert!((st.median_interval_fps - 100.0).abs() < 1e-9);
        assert!(st.measured_fps < 70.0);
    }

    #[test]
    fn no_sequence_numbers_means_unknown_sequence_drops() {
        let st = timing_stats(&frames(&[0, 10, 20], None), 100.0);
        assert_eq!(st.dropped_by_sequence, None);
    }

    #[test]
    fn convergence_and_settling() {
        let p = [100.0, 400.0, 900.0, 1000.0, 1010.0, 1005.0, 1000.0, 998.0];
        let samples: Vec<ExposureSample> = p
            .iter()
            .enumerate()
            .map(|(i, &p)| ExposureSample {
                at_ms: i as f64 * 10.0,
                exposure_product_us: Some(p),
                ae_state: Some(if i >= 5 { 2 } else { 1 }),
            })
            .collect();
        assert_eq!(first_converged_ms(&samples), Some(50.0));
        assert_eq!(settled_ms(&samples, 4, 0.02), Some(30.0));
        assert_eq!(settled_ms(&samples, 20, 0.02), None);
    }

    #[test]
    fn spread() {
        let s = Spread::of(&[3.0, 1.0, 2.0]).unwrap();
        assert_eq!((s.min, s.median, s.max, s.count), (1.0, 2.0, 3.0, 3));
        assert!(Spread::of(&[]).is_none());
    }
}
