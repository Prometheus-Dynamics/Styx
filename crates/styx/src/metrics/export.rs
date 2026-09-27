//! A [`HealthReport`] as flat metric samples, and as Prometheus text exposition.

use std::fmt::Write;

use super::{FrameDropReason, HealthReport};

/// How a metric behaves over time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    /// Only increases while the pipeline runs (drops, copies, reconnects).
    Counter,
    /// A current value (queue depth, latency percentile, frame rate).
    Gauge,
}

/// One reading of a [`HealthReport`], ready for any metrics system.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricSample {
    /// Prometheus-style name, e.g. `styx_frame_drops_total`.
    pub name: &'static str,
    pub help: &'static str,
    pub kind: MetricKind,
    /// Labels telling samples of the same metric apart, e.g. `("reason", "sensor_sequence_gap")`.
    pub labels: Vec<(&'static str, &'static str)>,
    pub value: f64,
}

impl FrameDropReason {
    /// Stable snake-case name, used as a metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CaptureQueueSendTimeout => "capture_queue_send_timeout",
            Self::CaptureQueueEviction => "capture_queue_eviction",
            Self::GraphDrop => "graph_drop",
            Self::GraphLatestReplacement => "graph_latest_replacement",
            Self::SensorSequenceGap => "sensor_sequence_gap",
        }
    }
}

const DROP_REASONS: [FrameDropReason; 5] = [
    FrameDropReason::CaptureQueueSendTimeout,
    FrameDropReason::CaptureQueueEviction,
    FrameDropReason::GraphDrop,
    FrameDropReason::GraphLatestReplacement,
    FrameDropReason::SensorSequenceGap,
];

impl HealthReport {
    /// Every numeric value of the report as metric samples. Values that are not known yet
    /// (e.g. latency before the first frame) are left out; drop counters are always present,
    /// zero when nothing was dropped for that reason.
    pub fn metric_samples(&self) -> Vec<MetricSample> {
        let mut out = Vec::new();
        let mut push = |name, help, kind, labels: Vec<_>, value: f64| {
            out.push(MetricSample {
                name,
                help,
                kind,
                labels,
                value,
            });
        };
        use MetricKind::{Counter, Gauge};
        if let Some(fps) = self.output_fps {
            push(
                "styx_output_fps",
                "Frames per second delivered.",
                Gauge,
                vec![],
                fps,
            );
        }
        push(
            "styx_capture_queue_depth",
            "Frames waiting between capture and the consumer.",
            Gauge,
            vec![],
            self.capture_queue_depth as f64,
        );
        push(
            "styx_capture_queue_capacity",
            "Capacity of the capture queue.",
            Gauge,
            vec![],
            self.capture_queue_capacity as f64,
        );
        push(
            "styx_capture_backpressure_total",
            "Times capture waited for room in the queue.",
            Counter,
            vec![],
            self.capture_backpressure_count as f64,
        );
        for reason in DROP_REASONS {
            let count = self
                .drop_reasons
                .iter()
                .filter(|d| d.reason == reason)
                .map(|d| d.count)
                .sum::<u64>();
            push(
                "styx_frame_drops_total",
                "Frames lost, by reason.",
                Counter,
                vec![("reason", reason.as_str())],
                count as f64,
            );
        }
        let stages = [
            ("end_to_end", self.latency_p50_ms, self.latency_p95_ms),
            (
                "source_to_sink",
                self.source_latency_p50_ms,
                self.source_latency_p95_ms,
            ),
            (
                "sensor_to_capture",
                self.sensor_latency_p50_ms,
                self.sensor_latency_p95_ms,
            ),
            (
                "capture_wait",
                self.capture_wait_p50_ms,
                self.capture_wait_p95_ms,
            ),
            ("decode", self.decode_p50_ms, self.decode_p95_ms),
            ("encode", self.encode_p50_ms, self.encode_p95_ms),
            ("sink", self.sink_p50_ms, self.sink_p95_ms),
        ];
        for (stage, p50, p95) in stages {
            for (quantile, value) in [("0.5", p50), ("0.95", p95)] {
                if let Some(value) = value {
                    push(
                        "styx_stage_latency_ms",
                        "Stage latency percentiles over the recent window, in milliseconds.",
                        Gauge,
                        vec![("stage", stage), ("quantile", quantile)],
                        value,
                    );
                }
            }
        }
        push(
            "styx_frame_copies_total",
            "Frame copies made by the pipeline.",
            Counter,
            vec![],
            self.copy_count as f64,
        );
        push(
            "styx_bytes_moved_total",
            "Bytes copied by the pipeline.",
            Counter,
            vec![],
            self.bytes_moved as f64,
        );
        push(
            "styx_external_inflight_buffers",
            "Driver or device buffers currently held by frames.",
            Gauge,
            vec![],
            self.external_inflight_buffers as f64,
        );
        push(
            "styx_external_inflight_bytes",
            "Bytes of driver or device buffers currently held by frames.",
            Gauge,
            vec![],
            self.external_inflight_bytes as f64,
        );
        let retries = &self.capture_retries;
        push(
            "styx_capture_start_retries_total",
            "Capture start attempts that failed and were retried.",
            Counter,
            vec![],
            retries.start_retry_count as f64,
        );
        push(
            "styx_reconnect_attempts_total",
            "Attempts to reconnect a disconnected or stalled source.",
            Counter,
            vec![],
            retries.reconnect_attempts as f64,
        );
        push(
            "styx_reconnects_total",
            "Times frames resumed after a reconnect.",
            Counter,
            vec![],
            retries.reconnects as f64,
        );
        if let Some(ms) = retries.last_reconnect_downtime_ms {
            push(
                "styx_last_reconnect_downtime_ms",
                "Time without frames around the last reconnect, in milliseconds.",
                Gauge,
                vec![],
                ms as f64,
            );
        }
        out
    }

    /// The report in Prometheus text exposition format, each sample labelled with `labels`
    /// (e.g. `[("camera", "front")]`). See [`render_prometheus`] for several cameras.
    pub fn to_prometheus(&self, labels: &[(&str, &str)]) -> String {
        render_prometheus([(labels, self)])
    }
}

/// Several reports (typically one per camera, told apart by `labels`) as one Prometheus text
/// exposition, with each metric's `# HELP` and `# TYPE` written once.
pub fn render_prometheus<'a>(
    reports: impl IntoIterator<Item = (&'a [(&'a str, &'a str)], &'a HealthReport)>,
) -> String {
    let mut groups: Vec<(MetricSample, Vec<String>)> = Vec::new();
    for (labels, report) in reports {
        for sample in report.metric_samples() {
            let mut all: Vec<(&str, &str)> = labels.to_vec();
            all.extend(sample.labels.iter().copied());
            let line = format!("{}{} {}", sample.name, label_set(&all), sample.value);
            match groups.iter_mut().find(|(s, _)| s.name == sample.name) {
                Some((_, lines)) => lines.push(line),
                None => groups.push((sample, vec![line])),
            }
        }
    }
    let mut out = String::new();
    for (sample, lines) in groups {
        let kind = match sample.kind {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
        };
        let _ = writeln!(out, "# HELP {} {}", sample.name, sample.help);
        let _ = writeln!(out, "# TYPE {} {kind}", sample.name);
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

fn label_set(labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let body: Vec<String> = labels
        .iter()
        .map(|(key, value)| {
            let escaped = value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            format!("{key}=\"{escaped}\"")
        })
        .collect();
    format!("{{{}}}", body.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::FrameDropStats;

    #[test]
    fn drops_and_latency_render_with_camera_labels() {
        let report = HealthReport {
            latency_p95_ms: Some(12.5),
            drop_reasons: vec![FrameDropStats {
                reason: FrameDropReason::SensorSequenceGap,
                count: 3,
            }],
            ..Default::default()
        };
        let idle = HealthReport::default();
        let text = render_prometheus([
            (&[("camera", "front")][..], &report),
            (&[("camera", "rear \"2\"")][..], &idle),
        ]);
        assert_eq!(
            text.matches("# TYPE styx_frame_drops_total counter")
                .count(),
            1
        );
        assert!(text.contains(
            "styx_frame_drops_total{camera=\"front\",reason=\"sensor_sequence_gap\"} 3\n"
        ));
        assert!(text.contains(
            "styx_frame_drops_total{camera=\"rear \\\"2\\\"\",reason=\"sensor_sequence_gap\"} 0\n"
        ));
        assert!(text.contains(
            "styx_stage_latency_ms{camera=\"front\",stage=\"end_to_end\",quantile=\"0.95\"} 12.5\n"
        ));
        // Unknown values are left out rather than reported as zero.
        assert!(!text.contains("styx_output_fps"));
    }
}
