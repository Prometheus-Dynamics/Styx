use std::sync::Arc;

use styx_codec::prelude::*;

#[cfg(feature = "hooks")]
use crate::recording::FrameRecorder;

#[cfg(feature = "hooks")]
use super::{FrameHookFn, HookFn, HookStore};
use crate::capture_api::{CaptureHandle, CaptureRequest, StyxConfig};
use crate::service::{SharedStyxServiceRuntime, StyxServiceConfig, StyxServiceRuntime};
use crate::session::runtime::MediaPipeline;

/// Builder for a capture→decode→hook→encode pipeline.
///
/// # Example
/// ```rust,no_run
/// use std::sync::Arc;
/// use styx::prelude::*;
///
/// let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
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
    execution_mode: PipelineExecutionMode,
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
    service_runtime: Option<SharedStyxServiceRuntime>,
}

/// Execution backend selected for a `MediaPipeline`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PipelineExecutionMode {
    /// Use the release default for the compiled feature set.
    ///
    /// Currently the linear runtime.
    #[default]
    Auto,
    /// Use direct capture→decode→hook→encode processing.
    Linear,
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
            execution_mode: PipelineExecutionMode::Auto,
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

    /// Use the release-default execution runtime for the compiled feature set.
    pub fn auto_execution(mut self) -> Self {
        self.execution_mode = PipelineExecutionMode::Auto;
        self
    }

    /// Use direct linear pipeline execution even when graph support is compiled in.
    pub fn linear_execution(mut self) -> Self {
        self.execution_mode = PipelineExecutionMode::Linear;
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

    /// Attach a decoder by looking it up in the registry.
    pub fn decoder_from_registry(
        mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<Self, RegistryError> {
        let decoder = super::runtime::lookup_codec(
            registry,
            CodecKind::Decoder,
            fourcc,
            impl_name,
            prefer_hardware,
        )?;
        self.decoder = Some(decoder);
        Ok(self)
    }

    /// Attach the registry's preferred decoder from `input` to `output`, e.g. MJPG → GREY picks a
    /// hardware luma decoder when one is available and `turbojpeg-luma` otherwise.
    pub fn decoder_for_output(
        mut self,
        registry: &CodecRegistryHandle,
        input: FourCc,
        output: FourCc,
    ) -> Result<Self, RegistryError> {
        self.decoder = Some(registry.lookup_for_output(input, output)?);
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
        let encoder = super::runtime::lookup_codec(
            registry,
            CodecKind::Encoder,
            fourcc,
            impl_name,
            prefer_hardware,
        )?;
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
        self.start_linear_with_policy(policy)
    }

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
}
