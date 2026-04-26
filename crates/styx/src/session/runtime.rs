use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(all(feature = "hooks", feature = "dynamic-image"))]
use image::DynamicImage;
use styx_codec::prelude::*;

#[cfg(feature = "hooks")]
use crate::recording::FrameRecorder;

#[cfg(feature = "hooks")]
use super::{FrameHookFn, HookFn, HookStore};
use crate::capture_api::CaptureHandle;

fn default_copy_required_for_transition(
    _from: FrameResidency,
    _to: FrameResidency,
    reason: ResidencyTransitionReason,
) -> bool {
    matches!(
        reason,
        ResidencyTransitionReason::PackedTransform
            | ResidencyTransitionReason::ImageMaterialize
            | ResidencyTransitionReason::ImageHook
            | ResidencyTransitionReason::BackendFallbackCopy
    )
}

fn annotate_residency_transition(
    metrics: &crate::metrics::ResidencyMetrics,
    frame: &mut FrameLease,
    from: FrameResidency,
    reason: ResidencyTransitionReason,
) {
    let copied = default_copy_required_for_transition(from, frame.residency(), reason);
    annotate_residency_transition_with_copy(metrics, frame, from, reason, copied);
}

fn annotate_residency_transition_with_copy(
    metrics: &crate::metrics::ResidencyMetrics,
    frame: &mut FrameLease,
    from: FrameResidency,
    reason: ResidencyTransitionReason,
    copied: bool,
) {
    let to = frame.residency();
    let transition = ResidencyTransition {
        from,
        to,
        reason,
        copied,
    };
    frame.meta_mut().residency = Some(to);
    frame.meta_mut().last_transition = Some(transition);
    metrics.record(transition);
}

fn stage_accepts_residency(accepted: &[FrameResidency], residency: FrameResidency) -> bool {
    accepted.contains(&residency)
}

/// Running pipeline session.
///
/// Use `next` or `next_blocking` to pull frames through the pipeline.
pub struct MediaPipeline {
    pub(super) capture: CaptureHandle,
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
}

impl MediaPipeline {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> RecvOutcome<FrameLease> {
        let span = tracing::trace_span!("capture_stage");
        let _enter = span.enter();
        let capture_start = Instant::now();
        match self.capture.recv() {
            RecvOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                self.process_frame(frame)
            }
            RecvOutcome::Empty => RecvOutcome::Empty,
            RecvOutcome::Closed => RecvOutcome::Closed,
        }
    }

    pub fn next_blocking(&mut self, wait: Duration) -> RecvOutcome<FrameLease> {
        let span = tracing::trace_span!("capture_stage", blocking = true);
        let _enter = span.enter();
        let capture_start = Instant::now();
        match self.capture.recv_timeout(wait) {
            styx_core::queue::RecvWaitOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                self.process_frame(frame)
            }
            styx_core::queue::RecvWaitOutcome::Closed => RecvOutcome::Closed,
            styx_core::queue::RecvWaitOutcome::Timeout => RecvOutcome::Empty,
        }
    }

    #[cfg(feature = "async")]
    pub async fn next_async(&mut self) -> RecvOutcome<FrameLease> {
        let span = tracing::trace_span!("capture_stage", async_mode = true);
        let _enter = span.enter();
        let capture_start = Instant::now();
        match self.capture.recv_async().await {
            RecvOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                self.process_frame(frame)
            }
            RecvOutcome::Empty => RecvOutcome::Empty,
            RecvOutcome::Closed => RecvOutcome::Closed,
        }
    }

    #[cfg(feature = "async")]
    pub fn spawn_async_worker(mut self) -> tokio::task::JoinHandle<()> {
        tokio::task::spawn(async move {
            loop {
                match self.next_async().await {
                    RecvOutcome::Data(_) => {}
                    RecvOutcome::Empty => {}
                    RecvOutcome::Closed => {
                        self.capture.stop_in_place();
                        break;
                    }
                }
            }
        })
    }

    pub fn capture(&self) -> &CaptureHandle {
        &self.capture
    }

    #[cfg(feature = "hooks")]
    pub fn output_recorder(&self) -> Option<&FrameRecorder> {
        self.output_recorder.as_ref()
    }

    #[cfg(feature = "hooks")]
    pub fn take_output_recorder(&mut self) -> Option<FrameRecorder> {
        self.output_recorder.take()
    }

    pub fn set_decoder(&mut self, decoder: Option<Arc<dyn Codec>>) {
        self.decoder = decoder;
    }

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

    pub fn set_encoder(&mut self, encoder: Option<Arc<dyn Codec>>) {
        self.encoder = encoder;
    }

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

    #[cfg(feature = "hooks")]
    pub fn set_hook<F>(&mut self, hook: Option<F>)
    where
        F: FnMut(FrameLease) -> FrameLease + Send + 'static,
    {
        self.hook = hook.map(|h| HookStore::Local(Some(Box::new(h) as HookFn)));
    }

    #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
    pub fn set_dynamic_hook<F>(&mut self, hook: Option<F>)
    where
        F: FnMut(DynamicImage) -> DynamicImage + Send + 'static,
    {
        self.hook = hook.map(|mut h| {
            HookStore::Local(Some(Box::new(move |img: FrameLease| {
                let ts = img.meta().timestamp;
                match img.into_dynamic_image() {
                    Ok(dynamic) => {
                        <FrameLease as FrameLeaseImageExt>::from_dynamic_image(h(dynamic), ts)
                            .expect("dynamic hook output must convert back into a frame")
                    }
                    Err(img) => img,
                }
            }) as HookFn))
        });
    }

    #[cfg(feature = "hooks")]
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

    pub fn stop(mut self) {
        self.capture.stop_in_place();
        self.cleanup_pools();
    }

    #[cfg(feature = "hooks")]
    pub fn stop_with_recorder(mut self) -> Option<FrameRecorder> {
        self.capture.stop_in_place();
        self.cleanup_pools();
        self.output_recorder.take()
    }

    pub fn set_capture(&mut self, capture: CaptureHandle) {
        let old = std::mem::replace(&mut self.capture, capture);
        old.stop();
    }

    pub fn enable_decode(&mut self, enabled: bool) {
        self.decode_enabled = enabled;
    }

    pub fn enable_encode(&mut self, enabled: bool) {
        self.encode_enabled = enabled;
    }

    #[cfg(target_os = "linux")]
    pub fn enable_shared_decode_output(&mut self, enabled: bool) {
        self.shared_decode_enabled = enabled;
    }

    #[cfg(target_os = "linux")]
    pub fn enable_owned_decode_fallback(&mut self, enabled: bool) {
        self.owned_decode_fallback_enabled = enabled;
    }

    #[cfg(target_os = "linux")]
    pub fn enable_shared_encode_output(&mut self, enabled: bool) {
        self.shared_encode_enabled = enabled;
    }

    #[cfg(target_os = "linux")]
    pub fn enable_owned_encode_fallback(&mut self, enabled: bool) {
        self.owned_encode_fallback_enabled = enabled;
    }

    #[cfg(feature = "hooks")]
    pub fn set_frame_transform(&mut self, transform: FrameTransform) {
        self.frame_transform = transform;
    }

    pub fn metrics(&self) -> crate::metrics::PipelineMetrics {
        self.metrics.clone()
    }

    pub fn health_report(&self) -> crate::metrics::HealthReport {
        let capture = self.capture.health_report();
        let end_to_end = self.metrics.end_to_end.snapshot();
        let source_to_sink = self.metrics.source_to_sink.snapshot();
        let copies = self.metrics.copies.snapshot();
        let memory = self.memory_stats();
        let residency = self.metrics.residency.snapshot();
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
        crate::metrics::HealthReport {
            output_fps: end_to_end.fps.or(capture.output_fps),
            capture_queue_depth: capture.capture_queue_depth,
            capture_queue_capacity: capture.capture_queue_capacity,
            capture_backpressure_count: capture.capture_backpressure_count,
            drop_count: capture.drop_count,
            capture_wait_p50_ms: capture.capture_wait_p50_ms,
            capture_wait_p95_ms: capture.capture_wait_p95_ms,
            latency_p50_ms: end_to_end.p50_millis,
            latency_p95_ms: end_to_end.p95_millis,
            source_latency_p50_ms: source_to_sink.p50_millis,
            source_latency_p95_ms: source_to_sink.p95_millis,
            copy_count: copies.copies,
            bytes_moved: copies.bytes_moved,
            external_inflight_buffers,
            external_inflight_bytes,
            recent_residency_transitions: residency.transitions,
        }
    }

    pub fn memory_stats(&self) -> crate::metrics::PipelineMemoryStats {
        let capture = self.capture.memory_stats();
        crate::metrics::PipelineMemoryStats {
            capture_queue: capture.capture_queue,
            external_backings: capture.external_backings,
            transform_pool: styx_core::transform::transform_pool_stats().or(capture.transform_pool),
            #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
            image_pool: styx_codec::image_utils::dynamic_image_pool_stats(),
            #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
            packed_pools: styx_codec::decoder::packed_frame_pool_stats(),
            #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
            staging_copy: Some(styx_codec::decoder::staging_copy_stats().into()),
        }
    }

    pub fn spawn_worker(mut self) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            loop {
                match self.next_blocking(Duration::from_millis(2)) {
                    RecvOutcome::Data(_) => {}
                    RecvOutcome::Empty => {}
                    RecvOutcome::Closed => {
                        self.capture.stop_in_place();
                        break;
                    }
                }
            }
        })
    }

    fn process_frame(&mut self, frame: FrameLease) -> RecvOutcome<FrameLease> {
        let pipeline_span = tracing::trace_span!("pipeline_frame");
        let _pipeline_enter = pipeline_span.enter();
        let pipeline_start = Instant::now();
        let source_capture_instant = frame.meta().capture_instant();
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
                match self.shared_decode_pool_for(&cur) {
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
                Err(_) => return RecvOutcome::Closed,
            }
        }
        #[cfg(feature = "hooks")]
        if let Some(hook) = &mut self.frame_hook {
            if cur.mutability() == FrameMutability::ReadOnly {
                let bytes = cur.payload_bytes();
                cur = cur.materialize_owned();
                self.metrics.copies.record_copy(bytes);
                annotate_residency_transition_with_copy(
                    &self.metrics.residency,
                    &mut cur,
                    current_residency,
                    ResidencyTransitionReason::FrameHook,
                    true,
                );
                current_residency = cur.residency();
            }
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
            let needs_image = self.hook.is_some() || !self.frame_transform.is_identity();
            if needs_image {
                let span = tracing::trace_span!("transform_stage", kind = "image_frame");
                let _enter = span.enter();
                let stage_bytes = cur.payload_bytes();
                let mut img = cur;
                if !self.frame_transform.is_identity() {
                    img = img.apply_image_transform(self.frame_transform);
                }
                if let Some(hook) = &mut self.hook {
                    let mut h = HookStore::take(hook);
                    img = (h)(img);
                    HookStore::put(hook, h);
                }
                cur = img;
                if self.hook.is_some() || !self.frame_transform.is_identity() {
                    self.metrics
                        .copies
                        .record_copy(stage_bytes.max(cur.payload_bytes()));
                }
                annotate_residency_transition_with_copy(
                    &self.metrics.residency,
                    &mut cur,
                    current_residency,
                    ResidencyTransitionReason::ImageHook,
                    self.hook.is_some() || !self.frame_transform.is_identity(),
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
                match self.shared_encode_pool_for(&cur) {
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
                Err(_) => return RecvOutcome::Closed,
            }
        }
        #[cfg(feature = "hooks")]
        if let Some(recorder) = &mut self.output_recorder {
            let span = tracing::trace_span!("sink_stage", kind = "record");
            let _enter = span.enter();
            let _ = recorder.record(&cur);
        }
        self.metrics.copies.record_output(&cur);
        self.metrics.end_to_end.record(pipeline_start.elapsed());
        if let Some(capture_instant) = source_capture_instant {
            self.metrics
                .source_to_sink
                .record(capture_instant.elapsed());
        }
        RecvOutcome::Data(cur)
    }

    #[cfg(target_os = "linux")]
    fn shared_decode_pool_for(
        &mut self,
        frame: &FrameLease,
    ) -> Result<&SharedBufferPool, FrameExportError> {
        let res = frame.meta().format.resolution;
        let bytes = (res.width.get() as usize)
            .saturating_mul(res.height.get() as usize)
            .saturating_mul(4)
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
        frame: &FrameLease,
    ) -> Result<&SharedBufferPool, FrameExportError> {
        let bytes = frame.payload_bytes().max(1 << 20);
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
        #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
        {
            styx_codec::decoder::clear_packed_frame_pools_all_threads();
            styx_codec::image_utils::reset_dynamic_image_pool();
        }
        styx_core::transform::reset_transform_pool();
    }
}

impl Drop for MediaPipeline {
    fn drop(&mut self) {
        self.capture.stop_in_place();
        self.cleanup_pools();
    }
}

#[cfg(target_os = "linux")]
fn require_exportable_codec_output(
    stage: &str,
    codec: &dyn Codec,
    frame: FrameLease,
    allow_owned_fallback: bool,
) -> Result<FrameLease, CodecError> {
    if allow_owned_fallback {
        return Ok(frame);
    }
    match frame.export_backing() {
        Ok(_) => Ok(frame),
        Err(err) => Err(CodecError::Codec(format!(
            "{stage} {}:{} produced non-exportable output: {err}",
            codec.descriptor().name,
            codec.descriptor().impl_name
        ))),
    }
}

pub(super) fn lookup_codec(
    registry: &CodecRegistryHandle,
    fourcc: FourCc,
    impl_name: Option<&str>,
    prefer_hardware: bool,
) -> Result<Arc<dyn Codec>, RegistryError> {
    if let Some(name) = impl_name {
        registry.lookup_named(fourcc, name)
    } else if prefer_hardware {
        registry.lookup_preferred(fourcc, &[], true)
    } else {
        registry.lookup_auto(fourcc)
    }
}

impl Iterator for MediaPipeline {
    type Item = FrameLease;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.next_blocking(Duration::ZERO) {
                RecvOutcome::Data(f) => return Some(f),
                RecvOutcome::Empty => continue,
                RecvOutcome::Closed => return None,
            }
        }
    }
}
