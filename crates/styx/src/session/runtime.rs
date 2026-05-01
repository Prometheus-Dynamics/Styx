use std::sync::Arc;
use std::time::Instant;

use styx_codec::prelude::*;

#[cfg(feature = "hooks")]
use crate::recording::FrameRecorder;

#[cfg(feature = "hooks")]
use super::{FrameHookFn, HookFn, HookStore};
use crate::capture_api::CaptureHandle;
use crate::metrics::{PipelineStage, PipelineStageError};
use crate::service::{PipelineWorkerEvent, PipelineWorkerStopReason, SharedStyxServiceRuntime};

mod codec_lookup;
#[cfg(feature = "graph-pipeline")]
mod graph;
mod iter;
mod residency;
#[cfg(test)]
mod tests;
mod worker;

pub(super) use codec_lookup::lookup_codec;
#[cfg(feature = "graph-pipeline")]
pub(super) use graph::GraphMediaRuntime;
#[cfg(feature = "graph-pipeline")]
use graph::summarize_graph_telemetry;
pub use iter::MediaPipelineFrameIter;
use residency::*;

/// Running pipeline session.
///
/// Use `try_next` or `next_blocking` to pull frames through the pipeline.
pub struct MediaPipeline {
    pub(super) capture: CaptureHandle,
    #[cfg(feature = "graph-pipeline")]
    pub(super) graph_runtime: Option<GraphMediaRuntime>,
    pub(super) decoder: Option<Arc<dyn Codec>>,
    pub(super) encoder: Option<Arc<dyn Codec>>,
    #[cfg(feature = "hooks")]
    pub(super) hook: Option<HookStore<HookFn>>,
    #[cfg(feature = "hooks")]
    pub(super) frame_hook: Option<HookStore<FrameHookFn>>,
    #[cfg(feature = "hooks")]
    pub(super) frame_transform: FrameTransform,
    #[cfg(feature = "hooks")]
    pub(super) output_recorder: Option<FrameRecorder>,
    #[cfg(feature = "hooks")]
    pub(super) output_recorder_sink_id: String,
    pub(super) metrics: crate::metrics::PipelineMetrics,
    pub(super) decode_enabled: bool,
    pub(super) encode_enabled: bool,
    #[cfg(target_os = "linux")]
    pub(super) shared_decode_enabled: bool,
    #[cfg(target_os = "linux")]
    pub(super) owned_decode_fallback_enabled: bool,
    #[cfg(target_os = "linux")]
    pub(super) shared_decode_pool: Option<(SharedBufferPool, usize)>,
    #[cfg(target_os = "linux")]
    pub(super) shared_encode_enabled: bool,
    #[cfg(target_os = "linux")]
    pub(super) owned_encode_fallback_enabled: bool,
    #[cfg(target_os = "linux")]
    pub(super) shared_encode_pool: Option<(SharedBufferPool, usize)>,
    pub(super) service_runtime: Option<SharedStyxServiceRuntime>,
    #[cfg(feature = "hooks")]
    pub(super) recorder_sink_started: bool,
}

impl MediaPipeline {
    pub fn capture(&self) -> &CaptureHandle {
        &self.capture
    }

    pub fn service_runtime(&self) -> Option<&SharedStyxServiceRuntime> {
        self.service_runtime.as_ref()
    }

    #[cfg(feature = "hooks")]
    pub fn output_recorder(&self) -> Option<&FrameRecorder> {
        self.output_recorder.as_ref()
    }

    #[cfg(feature = "hooks")]
    pub fn take_output_recorder(&mut self) -> Option<FrameRecorder> {
        self.emit_direct_recorder_stopped();
        self.output_recorder.take()
    }

    #[cfg(not(feature = "graph-pipeline"))]
    pub fn set_decoder(&mut self, decoder: Option<Arc<dyn Codec>>) {
        self.decoder = decoder;
    }

    #[cfg(not(feature = "graph-pipeline"))]
    pub fn set_decoder_from_registry(
        &mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<(), RegistryError> {
        self.decoder = Some(lookup_codec(registry, fourcc, impl_name, prefer_hardware)?);
        Ok(())
    }

    #[cfg(not(feature = "graph-pipeline"))]
    pub fn set_encoder(&mut self, encoder: Option<Arc<dyn Codec>>) {
        self.encoder = encoder;
    }

    #[cfg(not(feature = "graph-pipeline"))]
    pub fn set_encoder_from_registry(
        &mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<(), RegistryError> {
        self.encoder = Some(lookup_codec(registry, fourcc, impl_name, prefer_hardware)?);
        Ok(())
    }

    #[cfg(all(feature = "hooks", not(feature = "graph-pipeline")))]
    pub fn set_hook<F>(&mut self, hook: Option<F>)
    where
        F: FnMut(FrameLease) -> FrameLease + Send + 'static,
    {
        self.hook = hook.map(|h| HookStore::Local(Some(Box::new(h) as HookFn)));
    }

    #[cfg(all(feature = "hooks", not(feature = "graph-pipeline")))]
    pub fn set_frame_hook<F>(&mut self, hook: Option<F>)
    where
        F: FnMut(FrameLease) -> FrameLease + Send + 'static,
    {
        self.frame_hook = hook.map(|h| HookStore::Local(Some(Box::new(h) as FrameHookFn)));
    }

    pub fn reconfigure_capture(
        &mut self,
        request: crate::capture_api::CaptureRequest<'_>,
    ) -> Result<(), crate::capture_api::CaptureError> {
        self.capture.reconfigure_in_place(request)
    }

    /// Submit a capture control event through the graph-backed control stream.
    #[cfg(feature = "graph-pipeline")]
    pub fn submit_control_event(
        &mut self,
        event: crate::graph::StyxControlEvent,
    ) -> Result<crate::graph::StyxControlResult, crate::capture_api::CaptureError> {
        if let Some(graph) = &mut self.graph_runtime {
            return graph
                .submit_control_event(event)
                .map_err(crate::capture_api::CaptureError::Backend);
        }
        Err(crate::capture_api::CaptureError::Backend(
            "graph control stream is not available".into(),
        ))
    }

    pub fn stop(mut self) {
        #[cfg(feature = "hooks")]
        self.emit_direct_recorder_stopped();
        self.capture.stop_in_place();
        self.cleanup_pools();
    }

    #[cfg(feature = "hooks")]
    pub fn stop_with_recorder(mut self) -> Option<FrameRecorder> {
        self.emit_direct_recorder_stopped();
        self.capture.stop_in_place();
        self.cleanup_pools();
        self.output_recorder.take()
    }

    pub fn set_capture(&mut self, capture: CaptureHandle) {
        let old = std::mem::replace(&mut self.capture, capture);
        old.stop();
    }

    #[cfg(not(feature = "graph-pipeline"))]
    pub fn enable_decode(&mut self, enabled: bool) {
        self.decode_enabled = enabled;
    }

    #[cfg(not(feature = "graph-pipeline"))]
    pub fn enable_encode(&mut self, enabled: bool) {
        self.encode_enabled = enabled;
    }

    #[cfg(all(target_os = "linux", not(feature = "graph-pipeline")))]
    pub fn enable_shared_decode_output(&mut self, enabled: bool) {
        self.shared_decode_enabled = enabled;
    }

    #[cfg(all(target_os = "linux", not(feature = "graph-pipeline")))]
    pub fn enable_owned_decode_fallback(&mut self, enabled: bool) {
        self.owned_decode_fallback_enabled = enabled;
    }

    #[cfg(all(target_os = "linux", not(feature = "graph-pipeline")))]
    pub fn enable_shared_encode_output(&mut self, enabled: bool) {
        self.shared_encode_enabled = enabled;
    }

    #[cfg(all(target_os = "linux", not(feature = "graph-pipeline")))]
    pub fn enable_owned_encode_fallback(&mut self, enabled: bool) {
        self.owned_encode_fallback_enabled = enabled;
    }

    #[cfg(all(feature = "hooks", not(feature = "graph-pipeline")))]
    pub fn set_frame_transform(&mut self, transform: FrameTransform) {
        self.frame_transform = transform;
    }

    pub fn metrics(&self) -> crate::metrics::PipelineMetrics {
        self.metrics.clone()
    }

    #[cfg(feature = "graph-pipeline")]
    pub fn graph_telemetry(&self) -> Option<daedalus::runtime::ExecutionTelemetry> {
        self.graph_runtime
            .as_ref()
            .and_then(GraphMediaRuntime::last_telemetry)
            .cloned()
    }

    #[cfg(feature = "graph-pipeline")]
    pub fn graph_telemetry_stats(&self) -> Option<crate::metrics::GraphTelemetryStats> {
        self.graph_runtime
            .as_ref()
            .and_then(GraphMediaRuntime::last_telemetry)
            .map(summarize_graph_telemetry)
    }

    pub fn health_report(&self) -> crate::metrics::HealthReport {
        let capture = self.capture.health_report();
        let decode = self.metrics.decode.snapshot();
        let encode = self.metrics.encode.snapshot();
        let sink = self.metrics.sink.snapshot();
        let end_to_end = self.metrics.end_to_end.snapshot();
        let source_to_sink = self.metrics.source_to_sink.snapshot();
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
        #[cfg(feature = "graph-pipeline")]
        let graph = self.graph_telemetry_stats();
        #[cfg(not(feature = "graph-pipeline"))]
        let graph: Option<crate::metrics::GraphTelemetryStats> = None;
        let mut drop_reasons = capture.drop_reasons.clone();
        if let Some(graph) = &graph {
            crate::metrics::push_drop_reason(
                &mut drop_reasons,
                crate::metrics::FrameDropReason::GraphDrop,
                graph.drops,
            );
            crate::metrics::push_drop_reason(
                &mut drop_reasons,
                crate::metrics::FrameDropReason::GraphLatestReplacement,
                graph.latest_replacements,
            );
        }
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
            decode_p50_ms: decode.p50_millis,
            decode_p95_ms: decode.p95_millis,
            encode_p50_ms: encode.p50_millis,
            encode_p95_ms: encode.p95_millis,
            sink_p50_ms: sink.p50_millis,
            sink_p95_ms: sink.p95_millis,
            copy_count: copies.copies,
            bytes_moved: copies.bytes_moved + graph.as_ref().map(|g| g.copied_bytes).unwrap_or(0),
            external_inflight_buffers,
            external_inflight_bytes,
            recent_residency_transitions: residency.transitions,
            recent_stage_errors: stage_errors,
            drop_reasons,
            graph,
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

    /// Most recent decode, encode, graph, transform, or sink failure recorded by the pipeline.
    ///
    /// This is useful for callers using the infallible `try_next`, `next_blocking`,
    /// `next_forever`, or `next_async_receive` convenience methods, which map stage failures
    /// to `RecvOutcome::Closed` for iterator-style control flow.
    pub fn last_stage_error(&self) -> Option<PipelineStageError> {
        self.health_report().recent_stage_errors.pop()
    }

    pub fn memory_stats(&self) -> crate::metrics::PipelineMemoryStats {
        let capture = self.capture.memory_stats();
        crate::metrics::PipelineMemoryStats {
            capture_queue: capture.capture_queue,
            external_backings: capture.external_backings,
            transform_pool: styx_core::transform::transform_pool_stats().or(capture.transform_pool),
            #[cfg(target_os = "linux")]
            shared_decode_pool: self
                .shared_decode_pool
                .as_ref()
                .map(|(pool, _)| pool.stats()),
            #[cfg(target_os = "linux")]
            shared_encode_pool: self
                .shared_encode_pool
                .as_ref()
                .map(|(pool, _)| pool.stats()),
        }
    }

    fn record_stage_error(
        &self,
        stage: PipelineStage,
        component: impl Into<String>,
        message: impl Into<String>,
    ) -> PipelineStageError {
        let error = PipelineStageError {
            stage,
            component: component.into(),
            message: message.into(),
        };
        self.metrics.stage_errors.record(
            error.stage,
            error.component.clone(),
            error.message.clone(),
        );
        error
    }

    fn emit_pipeline_worker_stopped(&self, reason: PipelineWorkerStopReason) {
        if let Some(service) = &self.service_runtime
            && let Ok(mut service) = service.lock()
        {
            service.record_pipeline_event(PipelineWorkerEvent::Stopped { reason });
        }
    }

    fn process_frame_result(
        &mut self,
        frame: FrameLease,
    ) -> Result<FrameLease, PipelineStageError> {
        let pipeline_span = tracing::trace_span!("pipeline_frame");
        let _pipeline_enter = pipeline_span.enter();
        let pipeline_start = Instant::now();
        let source_capture_instant = frame.meta().capture_instant();
        #[cfg(feature = "graph-pipeline")]
        if let Some(graph) = &mut self.graph_runtime {
            self.metrics.copies.record_input(&frame);
            match graph.process(frame) {
                Ok(cur) => {
                    self.metrics.copies.record_output(&cur);
                    self.metrics.end_to_end.record(pipeline_start.elapsed());
                    if let Some(capture_instant) = source_capture_instant {
                        self.metrics
                            .source_to_sink
                            .record(capture_instant.elapsed());
                    }
                    return Ok(cur);
                }
                Err(err) => {
                    tracing::error!(
                        stage = %err.stage,
                        component = %err.component,
                        error = %err.message,
                        "graph-backed media pipeline failed"
                    );
                    return Err(self.record_stage_error(err.stage, err.component, err.message));
                }
            }
        }
        let mut cur = frame;
        let mut current_residency = cur.residency();
        self.metrics.copies.record_input(&cur);
        if self.decode_enabled
            && let Some(dec) = self.decoder.clone()
        {
            let capabilities = dec.residency_capabilities();
            if !stage_accepts_residency(capabilities.accepted_inputs, current_residency) {
                tracing::trace!(stage = "decode", residency = %current_residency, "decoder rejected frame residency");
            }
            let span = tracing::trace_span!("decode_stage");
            let _enter = span.enter();
            let t = Instant::now();
            #[cfg(target_os = "linux")]
            let decoded = if self.shared_decode_enabled {
                let allow_owned_fallback = self.owned_decode_fallback_enabled;
                match self.shared_decode_pool_for(dec.descriptor(), &cur) {
                    Ok(pool) => match dec.process_shared(&cur, pool) {
                        Ok(Some(frame)) => Ok(frame),
                        Ok(None) => dec.process(cur).and_then(|frame| {
                            require_exportable_codec_output(
                                "decoder",
                                dec.as_ref(),
                                frame,
                                allow_owned_fallback,
                            )
                        }),
                        Err(err) => Err(err),
                    },
                    Err(err) => Err(CodecError::Codec(err.to_string())),
                }
            } else {
                dec.process(cur)
            };
            #[cfg(not(target_os = "linux"))]
            let decoded = dec.process(cur);

            match decoded {
                Ok(f) => {
                    self.metrics.decode.record(t.elapsed());
                    cur = f;
                    if !stage_accepts_residency(capabilities.possible_outputs, cur.residency()) {
                        tracing::trace!(stage = "decode", output_residency = %cur.residency(), "decoder produced unexpected output residency");
                    }
                    annotate_residency_transition(
                        &self.metrics.residency,
                        &mut cur,
                        current_residency,
                        ResidencyTransitionReason::Decode,
                    );
                    current_residency = cur.residency();
                }
                Err(err) => {
                    let descriptor = dec.descriptor();
                    let component = format!("{}:{}", descriptor.name, descriptor.impl_name);
                    tracing::error!(
                        stage = "decode",
                        codec = %component,
                        error = %err,
                        "decode stage failed"
                    );
                    return Err(self.record_stage_error(
                        PipelineStage::Decode,
                        component,
                        err.to_string(),
                    ));
                }
            }
        }
        #[cfg(feature = "hooks")]
        if let Some(hook) = &mut self.frame_hook {
            let span = tracing::trace_span!("transform_stage", kind = "frame_hook");
            let _enter = span.enter();
            let mut h = HookStore::take(hook);
            cur = (h)(cur);
            HookStore::put(hook, h);
            annotate_residency_transition(
                &self.metrics.residency,
                &mut cur,
                current_residency,
                ResidencyTransitionReason::FrameHook,
            );
            current_residency = cur.residency();
        }
        #[cfg(feature = "hooks")]
        {
            if !self.frame_transform.is_identity() {
                let span = tracing::trace_span!("transform_stage", kind = "packed_frame_transform");
                let _enter = span.enter();
                let stage_bytes = cur.payload_bytes();
                match transform_packed_frame(&cur, self.frame_transform) {
                    Ok(mut transformed) => {
                        self.metrics
                            .copies
                            .record_copy(stage_bytes.max(transformed.payload_bytes()));
                        annotate_residency_transition_with_copy(
                            &self.metrics.residency,
                            &mut transformed,
                            current_residency,
                            ResidencyTransitionReason::PackedTransform,
                            true,
                        );
                        cur = transformed;
                        current_residency = cur.residency();
                    }
                    Err(err) => {
                        tracing::trace!(error = %err, "packed frame transform skipped");
                    }
                }
            }
            if let Some(hook) = &mut self.hook {
                let span = tracing::trace_span!("transform_stage", kind = "framelease_hook");
                let _enter = span.enter();
                let mut h = HookStore::take(hook);
                cur = (h)(cur);
                HookStore::put(hook, h);
                annotate_residency_transition(
                    &self.metrics.residency,
                    &mut cur,
                    current_residency,
                    ResidencyTransitionReason::FrameHook,
                );
                current_residency = cur.residency();
            }
        }
        if let Some(enc) = self.encoder.clone()
            && self.encode_enabled
        {
            let capabilities = enc.residency_capabilities();
            if !stage_accepts_residency(capabilities.accepted_inputs, current_residency) {
                tracing::trace!(stage = "encode", residency = %current_residency, "encoder rejected frame residency");
            }
            let span = tracing::trace_span!("encode_stage");
            let _enter = span.enter();
            let t = Instant::now();
            #[cfg(target_os = "linux")]
            let encoded = if self.shared_encode_enabled {
                let allow_owned_fallback = self.owned_encode_fallback_enabled;
                match self.shared_encode_pool_for(enc.descriptor(), &cur) {
                    Ok(pool) => match enc.process_shared(&cur, pool) {
                        Ok(Some(frame)) => Ok(frame),
                        Ok(None) => enc.process(cur).and_then(|frame| {
                            require_exportable_codec_output(
                                "encoder",
                                enc.as_ref(),
                                frame,
                                allow_owned_fallback,
                            )
                        }),
                        Err(err) => Err(err),
                    },
                    Err(err) => Err(CodecError::Codec(err.to_string())),
                }
            } else {
                enc.process(cur)
            };
            #[cfg(not(target_os = "linux"))]
            let encoded = enc.process(cur);

            match encoded {
                Ok(f) => {
                    self.metrics.encode.record(t.elapsed());
                    cur = f;
                    if !stage_accepts_residency(capabilities.possible_outputs, cur.residency()) {
                        tracing::trace!(stage = "encode", output_residency = %cur.residency(), "encoder produced unexpected output residency");
                    }
                    annotate_residency_transition(
                        &self.metrics.residency,
                        &mut cur,
                        current_residency,
                        ResidencyTransitionReason::Encode,
                    );
                }
                Err(err) => {
                    let descriptor = enc.descriptor();
                    let component = format!("{}:{}", descriptor.name, descriptor.impl_name);
                    tracing::error!(
                        stage = "encode",
                        codec = %component,
                        error = %err,
                        "encode stage failed"
                    );
                    return Err(self.record_stage_error(
                        PipelineStage::Encode,
                        component,
                        err.to_string(),
                    ));
                }
            }
        }
        #[cfg(feature = "hooks")]
        if let Some(recorder) = &mut self.output_recorder {
            let span = tracing::trace_span!("sink_stage", kind = "record");
            let _enter = span.enter();
            let t = Instant::now();
            let sequence = recorder.next_sequence();
            let session_id = recorder.metadata().session_id.clone();
            match recorder.record(&cur) {
                Ok(path) => self.emit_recording_frame_indexed(
                    session_id,
                    sequence,
                    path.display().to_string(),
                ),
                Err(err) => self.emit_direct_recorder_error(err.to_string()),
            }
            self.metrics.sink.record(t.elapsed());
        }
        self.metrics.copies.record_output(&cur);
        self.metrics.end_to_end.record(pipeline_start.elapsed());
        if let Some(capture_instant) = source_capture_instant {
            self.metrics
                .source_to_sink
                .record(capture_instant.elapsed());
        }
        Ok(cur)
    }

    #[cfg(target_os = "linux")]
    fn shared_decode_pool_for(
        &mut self,
        descriptor: &CodecDescriptor,
        frame: &FrameLease,
    ) -> Result<&SharedBufferPool, FrameExportError> {
        let bytes = estimate_shared_output_bytes(descriptor, frame)
            .unwrap_or_else(|| frame.payload_bytes().max(1))
            .max(1);
        let recreate = self
            .shared_decode_pool
            .as_ref()
            .map(|(_, capacity)| *capacity < bytes)
            .unwrap_or(true);
        if recreate {
            self.shared_decode_pool = Some((SharedBufferPool::with_limits(2, bytes, 4)?, bytes));
        }
        Ok(&self
            .shared_decode_pool
            .as_ref()
            .expect("shared decode pool initialized")
            .0)
    }

    #[cfg(target_os = "linux")]
    fn shared_encode_pool_for(
        &mut self,
        descriptor: &CodecDescriptor,
        frame: &FrameLease,
    ) -> Result<&SharedBufferPool, FrameExportError> {
        let bytes = estimate_shared_output_bytes(descriptor, frame)
            .unwrap_or_else(|| frame.payload_bytes().max(64 * 1024))
            .max(1);
        let recreate = self
            .shared_encode_pool
            .as_ref()
            .map(|(_, capacity)| *capacity < bytes)
            .unwrap_or(true);
        if recreate {
            self.shared_encode_pool = Some((SharedBufferPool::with_limits(2, bytes, 4)?, bytes));
        }
        Ok(&self
            .shared_encode_pool
            .as_ref()
            .expect("shared encode pool initialized")
            .0)
    }

    fn cleanup_pools(&self) {
        // Pipeline-local pools are owned by `self` and drop naturally. Process-wide pools remain
        // configured until callers explicitly reconfigure or reset them.
    }

    #[cfg(feature = "hooks")]
    fn emit_recording_frame_indexed(&self, session_id: String, sequence: u64, path: String) {
        if let Some(service) = &self.service_runtime
            && let Ok(mut service) = service.lock()
        {
            service.record_recording_event(crate::service::RecordingLifecycleEvent::FrameIndexed {
                session_id,
                sequence,
                path,
            });
        }
    }

    #[cfg(feature = "hooks")]
    fn emit_direct_recorder_error(&self, message: String) {
        if let Some(service) = &self.service_runtime
            && let Ok(mut service) = service.lock()
        {
            service.record_sink_event(crate::service::SinkLifecycleEvent::Error {
                sink_id: self.output_recorder_sink_id.clone(),
                kind: crate::service::SinkKind::Recorder,
                message,
            });
        }
    }

    #[cfg(feature = "hooks")]
    fn emit_direct_recorder_stopped(&mut self) {
        if !self.recorder_sink_started {
            return;
        }
        self.recorder_sink_started = false;
        if let Some(service) = &self.service_runtime
            && let Ok(mut service) = service.lock()
        {
            if let Some(recorder) = &self.output_recorder {
                service.record_recording_event(crate::service::RecordingLifecycleEvent::Stopped {
                    session_id: recorder.metadata().session_id.clone(),
                    frames: recorder.paths().len(),
                });
            }
            service.record_sink_event(crate::service::SinkLifecycleEvent::Stopped {
                sink_id: self.output_recorder_sink_id.clone(),
                kind: crate::service::SinkKind::Recorder,
            });
        }
    }
}

impl Drop for MediaPipeline {
    fn drop(&mut self) {
        #[cfg(feature = "hooks")]
        self.emit_direct_recorder_stopped();
        self.capture.stop_in_place();
        self.cleanup_pools();
    }
}

impl Iterator for MediaPipeline {
    type Item = FrameLease;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.next_forever() {
                RecvOutcome::Data(f) => return Some(f),
                RecvOutcome::Empty => continue,
                RecvOutcome::Closed => return None,
            }
        }
    }
}
