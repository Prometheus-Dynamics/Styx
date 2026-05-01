use std::sync::Arc;

use styx_codec::prelude::*;

#[cfg(feature = "hooks")]
use crate::recording::FrameRecorder;

#[cfg(feature = "hooks")]
use super::{FrameHookFn, HookFn, HookStore};
use crate::capture_api::{CaptureHandle, CaptureRequest, StyxConfig};
use crate::service::{SharedStyxServiceRuntime, StyxServiceConfig, StyxServiceRuntime};
#[cfg(feature = "graph-pipeline")]
use crate::session::runtime::GraphMediaRuntime;
use crate::session::runtime::MediaPipeline;

/// Builder for a capture→decode→hook→encode pipeline.
///
/// # Example
/// ```rust,no_run
/// use std::sync::Arc;
/// use styx::prelude::*;
///
/// let device = make_virtual_rgb_device("virtual", 640, 360, 30);
/// let decoder = Arc::new(PassthroughDecoder::new(
///     device.backends[0].descriptor.modes[0].format.code,
/// ));
/// let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
///     .decoder(decoder)
///     .start()?;
///
/// loop {
///     match pipeline.try_next_result()? {
///         RecvOutcome::Data(frame) => println!("frame {:?}", frame.meta().format),
///         RecvOutcome::Empty | RecvOutcome::Closed => break,
///     }
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct MediaPipelineBuilder<'a> {
    capture: CaptureRequest<'a>,
    decoder: Option<Arc<dyn Codec>>,
    encoder: Option<Arc<dyn Codec>>,
    #[cfg(feature = "hooks")]
    hook: Option<HookStore<HookFn>>,
    #[cfg(feature = "hooks")]
    frame_hook: Option<HookStore<FrameHookFn>>,
    #[cfg(feature = "hooks")]
    frame_transform: FrameTransform,
    #[cfg(feature = "hooks")]
    output_recorder: Option<FrameRecorder>,
    #[cfg(feature = "hooks")]
    output_recorder_sink_id: Option<String>,
    decode_enabled: bool,
    encode_enabled: bool,
    #[cfg(target_os = "linux")]
    shared_decode_enabled: bool,
    #[cfg(target_os = "linux")]
    owned_decode_fallback_enabled: bool,
    #[cfg(target_os = "linux")]
    shared_encode_enabled: bool,
    #[cfg(target_os = "linux")]
    owned_encode_fallback_enabled: bool,
    #[cfg(feature = "graph-pipeline")]
    graph_metrics_level: daedalus::engine::MetricsLevel,
    service_runtime: Option<SharedStyxServiceRuntime>,
}

impl<'a> MediaPipelineBuilder<'a> {
    /// Start from a capture request.
    ///
    /// Use `CaptureRequest` to select backend/mode/controls before wiring
    /// the pipeline.
    pub fn new(capture: CaptureRequest<'a>) -> Self {
        Self {
            capture,
            decoder: None,
            encoder: None,
            #[cfg(feature = "hooks")]
            hook: None,
            #[cfg(feature = "hooks")]
            frame_hook: None,
            #[cfg(feature = "hooks")]
            frame_transform: FrameTransform::default(),
            #[cfg(feature = "hooks")]
            output_recorder: None,
            #[cfg(feature = "hooks")]
            output_recorder_sink_id: None,
            decode_enabled: true,
            encode_enabled: true,
            #[cfg(target_os = "linux")]
            shared_decode_enabled: true,
            #[cfg(target_os = "linux")]
            owned_decode_fallback_enabled: false,
            #[cfg(target_os = "linux")]
            shared_encode_enabled: true,
            #[cfg(target_os = "linux")]
            owned_encode_fallback_enabled: false,
            #[cfg(feature = "graph-pipeline")]
            graph_metrics_level: daedalus::engine::MetricsLevel::Basic,
            service_runtime: None,
        }
    }

    /// Attach a decoder.
    ///
    /// The decoder receives frames from capture and should output the
    /// desired pixel format for hooks/encoders.
    pub fn decoder(mut self, codec: Arc<dyn Codec>) -> Self {
        self.decoder = Some(codec);
        self
    }

    /// Attach an encoder.
    ///
    /// Encoders run after hooks to produce compressed output.
    pub fn encoder(mut self, codec: Arc<dyn Codec>) -> Self {
        self.encoder = Some(codec);
        self
    }

    /// Attach a recorder sink to the final output frames.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn sink(mut self, name: impl Into<String>, recorder: FrameRecorder) -> Self {
        self.output_recorder_sink_id = Some(name.into());
        self.output_recorder = Some(recorder);
        self
    }

    pub fn service_runtime(mut self, service: SharedStyxServiceRuntime) -> Self {
        self.service_runtime = Some(service);
        self
    }

    /// Create and attach a service runtime with explicit event retention settings.
    ///
    /// Use `service_runtime` instead when the application needs to subscribe before
    /// the pipeline starts or share one runtime across multiple sessions.
    pub fn service_runtime_config(mut self, config: StyxServiceConfig) -> Self {
        self.service_runtime = Some(Arc::new(std::sync::Mutex::new(
            StyxServiceRuntime::with_config(config),
        )));
        self
    }

    /// Use pipeline-local capture/runtime tunables instead of defaults.
    pub fn config(mut self, config: StyxConfig) -> Self {
        self.capture = self.capture.config(config);
        self
    }

    /// Toggle whether decode runs.
    ///
    /// Disabling decode can be useful when capture already produces the
    /// desired format.
    pub fn decode_enabled(mut self, enabled: bool) -> Self {
        self.decode_enabled = enabled;
        self
    }

    /// Toggle whether encode runs.
    ///
    /// Disabling encode yields the post-hook frame as the output.
    pub fn encode_enabled(mut self, enabled: bool) -> Self {
        self.encode_enabled = enabled;
        self
    }

    /// Skip the decode stage and pass captured frames to the next stage.
    pub fn without_decoder(mut self) -> Self {
        self.decode_enabled = false;
        self
    }

    /// Skip the encode stage and return decoded/transformed frames.
    pub fn without_encoder(mut self) -> Self {
        self.encode_enabled = false;
        self
    }

    /// Return captured frames exactly as the backend produced them.
    ///
    /// This is the fast path for preview, recording, hardware handoff, and
    /// diagnostics that should not force decode or encode work.
    pub fn raw_frames(mut self) -> Self {
        self = self.without_decoder();
        self.without_encoder()
    }

    #[cfg(target_os = "linux")]
    pub fn shared_decode_output(mut self, enabled: bool) -> Self {
        self.shared_decode_enabled = enabled;
        self
    }

    #[cfg(target_os = "linux")]
    pub fn owned_decode_fallback(mut self, enabled: bool) -> Self {
        self.owned_decode_fallback_enabled = enabled;
        self
    }

    #[cfg(target_os = "linux")]
    pub fn shared_encode_output(mut self, enabled: bool) -> Self {
        self.shared_encode_enabled = enabled;
        self
    }

    #[cfg(target_os = "linux")]
    pub fn owned_encode_fallback(mut self, enabled: bool) -> Self {
        self.owned_encode_fallback_enabled = enabled;
        self
    }

    /// Set the Daedalus metrics level used by graph-backed pipelines.
    ///
    /// The default is `Basic` to keep production frame latency low. Use
    /// `Detailed` or higher for profiling node/edge timing and transport
    /// byte counters.
    #[cfg(feature = "graph-pipeline")]
    pub fn graph_metrics_level(mut self, level: daedalus::engine::MetricsLevel) -> Self {
        self.graph_metrics_level = level;
        self
    }

    /// Attach a decoder by looking it up in the registry.
    pub fn decoder_from_registry(
        mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<Self, RegistryError> {
        let decoder = super::runtime::lookup_codec(registry, fourcc, impl_name, prefer_hardware)?;
        self.decoder = Some(decoder);
        Ok(self)
    }

    /// Attach an encoder by looking it up in the registry.
    pub fn encoder_from_registry(
        mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<Self, RegistryError> {
        let encoder = super::runtime::lookup_codec(registry, fourcc, impl_name, prefer_hardware)?;
        self.encoder = Some(encoder);
        Ok(self)
    }

    /// Attach a `FrameLease` hook between decode and encode.
    ///
    /// `FrameLease` keeps native frame metadata, layout, stride, and residency
    /// visible to the hook so callers can adapt to the incoming format instead
    /// of forcing eager conversion into one canonical image type.
    #[cfg(feature = "hooks")]
    pub fn hook<F>(mut self, hook: F) -> Self
    where
        F: FnMut(FrameLease) -> FrameLease + Send + 'static,
    {
        self.hook = Some(HookStore::Local(Some(Box::new(hook))));
        self
    }

    /// Attach a frame-level hook that works on `FrameLease` without image conversion.
    #[cfg(feature = "hooks")]
    pub fn frame_hook<F>(mut self, hook: F) -> Self
    where
        F: FnMut(FrameLease) -> FrameLease + Send + 'static,
    {
        self.frame_hook = Some(HookStore::Local(Some(Box::new(hook))));
        self
    }

    /// Apply a fixed frame transform between decode and encode.
    #[cfg(feature = "hooks")]
    pub fn frame_transform(mut self, transform: FrameTransform) -> Self {
        self.frame_transform = transform;
        self
    }

    /// Rotate the stream in 90-degree steps.
    #[cfg(feature = "hooks")]
    pub fn rotate(mut self, rotation: Rotation90) -> Self {
        self.frame_transform.rotation = rotation;
        self
    }

    /// Mirror the stream horizontally.
    #[cfg(feature = "hooks")]
    pub fn mirror(mut self, mirror: bool) -> Self {
        self.frame_transform.mirror = mirror;
        self
    }

    /// Start the pipeline.
    pub fn start(self) -> Result<MediaPipeline, crate::capture_api::CaptureError> {
        self.start_with_policy(crate::capture_api::CaptureStartPolicy::default())
    }

    /// Start the pipeline using a capture start policy.
    pub fn start_with_policy(
        self,
        policy: crate::capture_api::CaptureStartPolicy,
    ) -> Result<MediaPipeline, crate::capture_api::CaptureError> {
        #[cfg(feature = "graph-pipeline")]
        {
            self.start_graph_backed_with_policy(policy)
        }

        #[cfg(not(feature = "graph-pipeline"))]
        {
            self.start_linear_with_policy(policy)
        }
    }

    #[cfg(not(feature = "graph-pipeline"))]
    fn start_linear_with_policy(
        self,
        policy: crate::capture_api::CaptureStartPolicy,
    ) -> Result<MediaPipeline, crate::capture_api::CaptureError> {
        let capture: CaptureHandle = self.capture.start_with_policy(policy)?;
        #[cfg(feature = "hooks")]
        let recorder_sink_started = self.output_recorder.is_some();
        #[cfg(feature = "hooks")]
        let output_recorder_sink_id = self
            .output_recorder_sink_id
            .unwrap_or_else(|| "recording".to_string());
        #[cfg(feature = "hooks")]
        if let (Some(service), Some(recorder)) = (&self.service_runtime, &self.output_recorder)
            && let Ok(mut service) = service.lock()
        {
            service.record_sink_event(crate::service::SinkLifecycleEvent::Started {
                sink_id: output_recorder_sink_id.clone(),
                kind: crate::service::SinkKind::Recorder,
            });
            service.record_recording_event(crate::service::RecordingLifecycleEvent::Started {
                session_id: recorder.metadata().session_id.clone(),
                directory: recorder.metadata().directory.display().to_string(),
            });
        }
        Ok(MediaPipeline {
            capture,
            decoder: self.decoder,
            encoder: self.encoder,
            #[cfg(feature = "hooks")]
            hook: self.hook,
            #[cfg(feature = "hooks")]
            frame_hook: self.frame_hook,
            #[cfg(feature = "hooks")]
            frame_transform: self.frame_transform,
            #[cfg(feature = "hooks")]
            output_recorder: self.output_recorder,
            #[cfg(feature = "hooks")]
            output_recorder_sink_id,
            metrics: crate::metrics::PipelineMetrics::default(),
            decode_enabled: self.decode_enabled,
            encode_enabled: self.encode_enabled,
            #[cfg(target_os = "linux")]
            shared_decode_enabled: self.shared_decode_enabled,
            #[cfg(target_os = "linux")]
            owned_decode_fallback_enabled: self.owned_decode_fallback_enabled,
            #[cfg(target_os = "linux")]
            shared_decode_pool: None,
            #[cfg(target_os = "linux")]
            shared_encode_enabled: self.shared_encode_enabled,
            #[cfg(target_os = "linux")]
            owned_encode_fallback_enabled: self.owned_encode_fallback_enabled,
            #[cfg(target_os = "linux")]
            shared_encode_pool: None,
            service_runtime: self.service_runtime,
            #[cfg(feature = "hooks")]
            recorder_sink_started,
        })
    }

    #[cfg(feature = "graph-pipeline")]
    fn start_graph_backed_with_policy(
        self,
        policy: crate::capture_api::CaptureStartPolicy,
    ) -> Result<MediaPipeline, crate::capture_api::CaptureError> {
        let capture_request = self.capture.clone();
        let service_runtime = self.service_runtime.clone();
        let decode_enabled = self.decode_enabled;
        let encode_enabled = self.encode_enabled;
        #[cfg(target_os = "linux")]
        let shared_decode_enabled = self.shared_decode_enabled;
        #[cfg(target_os = "linux")]
        let owned_decode_fallback_enabled = self.owned_decode_fallback_enabled;
        #[cfg(target_os = "linux")]
        let shared_encode_enabled = self.shared_encode_enabled;
        #[cfg(target_os = "linux")]
        let owned_encode_fallback_enabled = self.owned_encode_fallback_enabled;
        let capture: CaptureHandle = capture_request.start_with_policy(policy)?;
        let graph_runtime = self.build_graph_runtime(capture.control.clone())?;
        Ok(MediaPipeline {
            capture,
            graph_runtime,
            decoder: None,
            encoder: None,
            #[cfg(feature = "hooks")]
            hook: None,
            #[cfg(feature = "hooks")]
            frame_hook: None,
            #[cfg(feature = "hooks")]
            frame_transform: FrameTransform::default(),
            #[cfg(feature = "hooks")]
            output_recorder: None,
            #[cfg(feature = "hooks")]
            output_recorder_sink_id: "recording".into(),
            metrics: crate::metrics::PipelineMetrics::default(),
            decode_enabled,
            encode_enabled,
            #[cfg(target_os = "linux")]
            shared_decode_enabled,
            #[cfg(target_os = "linux")]
            owned_decode_fallback_enabled,
            #[cfg(target_os = "linux")]
            shared_decode_pool: None,
            #[cfg(target_os = "linux")]
            shared_encode_enabled,
            #[cfg(target_os = "linux")]
            owned_encode_fallback_enabled,
            #[cfg(target_os = "linux")]
            shared_encode_pool: None,
            service_runtime,
            #[cfg(feature = "hooks")]
            recorder_sink_started: false,
        })
    }

    #[cfg(feature = "graph-pipeline")]
    fn build_graph_runtime(
        self,
        control: crate::capture_api::ControlPlane,
    ) -> Result<Option<GraphMediaRuntime>, crate::capture_api::CaptureError> {
        let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
        crate::graph::register_framelease_type();
        crate::graph::register_control_types();
        let mut media = crate::graph::StyxMediaPlugin::new();
        if let Some(service) = &self.service_runtime {
            media.set_service_runtime(service.clone());
        }
        let mut nodes = Vec::<daedalus::NodeHandle>::new();
        let control_node = crate::graph::register_capture_control_node(
            &mut registry,
            "styx.pipeline.capture_control",
            control,
        )
        .map_err(graph_start_error)?
        .alias("capture_control");

        if self.decode_enabled
            && let Some(decoder) = self.decoder
        {
            let decoder_node = daedalus::NodeHandle::new(crate::graph::concrete_codec_node_id(
                decoder.descriptor(),
            ))
            .alias("decode");
            let options = codec_node_options(
                #[cfg(target_os = "linux")]
                self.shared_decode_enabled,
                #[cfg(target_os = "linux")]
                self.owned_decode_fallback_enabled,
            );
            media.add_codec(decoder, options);
            nodes.push(decoder_node);
        }

        #[cfg(feature = "hooks")]
        if let Some(frame_hook) = self.frame_hook {
            nodes.push(
                register_hook_store_node(&mut media, "styx.pipeline.frame_hook", frame_hook)
                    .alias("frame_hook"),
            );
        }

        #[cfg(feature = "hooks")]
        if !self.frame_transform.is_identity() {
            nodes.push(
                media
                    .add_transform("styx.pipeline.transform", self.frame_transform)
                    .alias("transform"),
            );
        }

        #[cfg(feature = "hooks")]
        if let Some(hook) = self.hook {
            nodes.push(
                register_hook_store_node(&mut media, "styx.pipeline.hook", hook).alias("hook"),
            );
        }

        if self.encode_enabled
            && let Some(encoder) = self.encoder
        {
            let encoder_node = daedalus::NodeHandle::new(crate::graph::concrete_codec_node_id(
                encoder.descriptor(),
            ))
            .alias("encode");
            let options = codec_node_options(
                #[cfg(target_os = "linux")]
                self.shared_encode_enabled,
                #[cfg(target_os = "linux")]
                self.owned_encode_fallback_enabled,
            );
            media.add_codec(encoder, options);
            nodes.push(encoder_node);
        }

        #[cfg(feature = "hooks")]
        if let Some(recorder) = self.output_recorder {
            let sink_id = self
                .output_recorder_sink_id
                .unwrap_or_else(|| "recording".to_string());
            nodes.push(
                media
                    .add_recorder_sink(sink_id.clone(), recorder)
                    .alias(sink_id),
            );
        }

        let frame_path_enabled = !nodes.is_empty();
        registry.install(&media).map_err(graph_start_error)?;

        let graph = registry
            .graph_builder()
            .map_err(graph_start_error)?
            .inputs(|g| {
                g.input("frame");
                g.input("control");
            })
            .outputs(|g| {
                g.output("frame");
                g.output("control_result");
            })
            .nodes(|g| {
                g.add_handle(&control_node);
                for node in &nodes {
                    g.add_handle(node);
                }
            })
            .edges(|g| {
                g.connect("control", &control_node.input("control"));
                g.connect(&control_node.output("control_result"), "control_result");
                for (idx, node) in nodes.iter().enumerate() {
                    let input = node.input("frame");
                    let output = node.output("frame");
                    if idx == 0 {
                        g.connect("frame", &input);
                    } else {
                        let prev = nodes[idx - 1].output("frame");
                        g.connect(&prev, &input);
                    }
                    if idx + 1 == nodes.len() {
                        g.connect(&output, "frame");
                    }
                }
            })
            .build();
        let engine = daedalus::engine::Engine::new(
            daedalus::engine::EngineConfig::from(daedalus::engine::GpuBackend::Cpu)
                .with_metrics_level(self.graph_metrics_level),
        )
        .map_err(|err| graph_start_error(err.to_string()))?;
        let runtime = engine
            .compile_registry(&registry, graph)
            .map_err(|err| graph_start_error(err.to_string()))?;
        Ok(Some(GraphMediaRuntime::new(
            runtime,
            frame_path_enabled,
            self.service_runtime,
        )))
    }
}

#[cfg(feature = "graph-pipeline")]
fn graph_start_error(err: impl std::fmt::Display) -> crate::capture_api::CaptureError {
    crate::capture_api::CaptureError::Backend(format!("graph pipeline start failed: {err}"))
}

#[cfg(all(feature = "graph-pipeline", target_os = "linux"))]
fn codec_node_options(
    shared_output: bool,
    owned_fallback: bool,
) -> crate::graph::StyxCodecNodeOptions {
    crate::graph::StyxCodecNodeOptions {
        shared_output,
        owned_fallback,
    }
}

#[cfg(all(feature = "graph-pipeline", not(target_os = "linux")))]
fn codec_node_options() -> crate::graph::StyxCodecNodeOptions {
    crate::graph::StyxCodecNodeOptions::default()
}

#[cfg(all(feature = "graph-pipeline", feature = "hooks"))]
fn register_hook_store_node<T>(
    media: &mut crate::graph::StyxMediaPlugin,
    node_id: impl Into<String>,
    mut hook: HookStore<T>,
) -> daedalus::NodeHandle
where
    T: FnMut(FrameLease) -> FrameLease + Send + 'static,
{
    media.add_frame_hook(node_id, move |frame| {
        let mut hook_fn = HookStore::take(&mut hook);
        let out = hook_fn(frame);
        HookStore::put(&mut hook, hook_fn);
        out
    })
}

#[cfg(all(test, feature = "graph-pipeline"))]
mod tests {
    use super::*;
    use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};
    use std::num::NonZeroU32;
    use styx_capture::prelude::{
        CaptureDescriptor, ColorSpace, FourCc, Interval, MediaFormat, Mode, ModeId, RecvOutcome,
        Resolution,
    };

    fn virtual_device() -> ProbedDevice {
        let interval = Interval {
            numerator: NonZeroU32::new(1).unwrap(),
            denominator: NonZeroU32::new(30).unwrap(),
        };
        let format = MediaFormat::new(
            FourCc::RG24,
            Resolution::new(2, 2).unwrap(),
            ColorSpace::Srgb,
        );
        let mode = Mode {
            id: ModeId {
                format,
                interval: Some(interval),
            },
            format,
            intervals: smallvec::smallvec![interval],
            interval_stepwise: None,
        };
        ProbedDevice {
            identity: DeviceIdentity {
                display: "virtual-test".into(),
                keys: vec!["virtual-test".into()],
            },
            backends: vec![ProbedBackend {
                kind: BackendKind::Virtual,
                handle: BackendHandle::Virtual,
                descriptor: CaptureDescriptor {
                    modes: vec![mode],
                    controls: vec![],
                },
                properties: vec![],
            }],
        }
    }

    #[test]
    fn builder_start_runs_linear_facade_through_graph_pipeline() {
        let device = virtual_device();
        let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
        let mut pipeline = MediaPipelineBuilder::new(request)
            .decoder(Arc::new(PassthroughDecoder::new(FourCc::RG24)))
            .shared_decode_output(false)
            .without_encoder()
            .start()
            .expect("start graph-backed pipeline");

        match pipeline.next_blocking(std::time::Duration::from_millis(250)) {
            RecvOutcome::Data(frame) => {
                assert_eq!(frame.meta().format.code, FourCc::RG24);
            }
            RecvOutcome::Empty => panic!("expected frame from graph-backed pipeline, got empty"),
            RecvOutcome::Closed => panic!("expected frame from graph-backed pipeline, got closed"),
        }
        let control_result = pipeline
            .submit_control_event(crate::graph::StyxControlEvent::Get {
                id: styx_core::prelude::ControlId(1),
            })
            .expect("control event should route alongside frame nodes");
        assert!(!control_result.is_ok());
        assert!(pipeline.graph_telemetry().is_some());
    }

    #[test]
    fn graph_backed_pipeline_routes_control_events_through_graph() {
        let device = virtual_device();
        let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
        let service = Arc::new(std::sync::Mutex::new(
            crate::service::StyxServiceRuntime::new(),
        ));
        let mut cursor = service.lock().expect("service lock").subscribe_from_start();
        let mut pipeline = MediaPipelineBuilder::new(request)
            .raw_frames()
            .service_runtime(Arc::clone(&service))
            .start()
            .expect("start graph-backed raw pipeline");

        let result = pipeline
            .submit_control_event(crate::graph::StyxControlEvent::Set {
                id: styx_core::prelude::ControlId(1),
                value: styx_core::prelude::ControlValue::Bool(true),
            })
            .expect("control event routed through graph");

        assert!(!result.is_ok());
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("control plane not available"))
        );
        let _ = pipeline.health_report();
        let events = {
            let service = service.lock().expect("service lock");
            service.poll_events(&mut cursor).events().to_vec()
        };
        assert!(events.iter().any(|event| {
            matches!(
                event.event,
                crate::service::StyxServiceEvent::Control(crate::graph::StyxControlResult { .. })
            )
        }));
        assert!(
            events.iter().any(|event| {
                matches!(event.event, crate::service::StyxServiceEvent::Health(_))
            })
        );
        assert!(pipeline.graph_telemetry().is_some());
    }
}
