//! Pipeline health report assembly.

use super::MediaPipeline;

impl MediaPipeline {
    pub fn health_report(&self) -> crate::metrics::HealthReport {
        let capture = self.capture.health_report();
        let decode = self.metrics.decode.snapshot();
        let encode = self.metrics.encode.snapshot();
        let sink = self.metrics.sink.snapshot();
        let end_to_end = self.metrics.end_to_end.snapshot();
        let source_to_sink = self.metrics.source_to_sink.snapshot();
        let sensor_to_capture = self.metrics.sensor_to_capture.snapshot();
        let copies = self.metrics.copies.snapshot();
        let memory = self.memory_stats();
        let residency = self.metrics.residency.snapshot();
        let mut stage_errors = capture.recent_stage_errors.clone();
        stage_errors.extend(self.metrics.stage_errors.snapshot());
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
        let drop_reasons = capture.drop_reasons.clone();
        let drop_count = crate::metrics::total_frame_drops(&drop_reasons);
        let report = crate::metrics::HealthReport {
            output_fps: end_to_end.fps.or(capture.output_fps),
            capture_queue_depth: capture.capture_queue_depth,
            capture_queue_capacity: capture.capture_queue_capacity,
            capture_backpressure_count: capture.capture_backpressure_count,
            drop_count,
            capture_async_send_waits: capture.capture_async_send_waits,
            capture_async_recv_waits: capture.capture_async_recv_waits,
            capture_async_send_wakes: capture.capture_async_send_wakes,
            capture_async_recv_wakes: capture.capture_async_recv_wakes,
            capture_wait_p50_ms: capture.capture_wait_p50_ms,
            capture_wait_p95_ms: capture.capture_wait_p95_ms,
            latency_p50_ms: end_to_end.p50_millis,
            latency_p95_ms: end_to_end.p95_millis,
            source_latency_p50_ms: source_to_sink.p50_millis,
            source_latency_p95_ms: source_to_sink.p95_millis,
            sensor_latency_p50_ms: sensor_to_capture.p50_millis,
            sensor_latency_p95_ms: sensor_to_capture.p95_millis,
            decode_p50_ms: decode.p50_millis,
            decode_p95_ms: decode.p95_millis,
            encode_p50_ms: encode.p50_millis,
            encode_p95_ms: encode.p95_millis,
            sink_p50_ms: sink.p50_millis,
            sink_p95_ms: sink.p95_millis,
            copy_count: copies.copies,
            bytes_moved: copies.bytes_moved,
            external_inflight_buffers,
            external_inflight_bytes,
            recent_residency_transitions: residency.transitions,
            recent_stage_errors: stage_errors,
            drop_reasons,
            capture_shutdown: capture.capture_shutdown,
            capture_retries: capture.capture_retries,
        };
        if let Some(service) = &self.service_runtime
            && let Ok(mut service) = service.lock()
        {
            service.record_health(report.clone());
        }
        report
    }
}
