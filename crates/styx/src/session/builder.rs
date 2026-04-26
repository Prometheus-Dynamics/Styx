use std::sync::Arc;

#[cfg(all(feature = "hooks", feature = "dynamic-image"))]
use image::DynamicImage;
use styx_codec::prelude::*;

#[cfg(feature = "hooks")]
use crate::recording::FrameRecorder;

#[cfg(feature = "hooks")]
use super::{FrameHookFn, HookFn, HookStore};
use crate::capture_api::{CaptureHandle, CaptureRequest};
use crate::session::runtime::MediaPipeline;

/// Builder for a capture→decode→hook→encode pipeline.
///
/// # Example
/// ```rust,ignore
/// use std::sync::Arc;
/// use styx::prelude::*;
///
/// let device = probe_all().into_iter().next().expect("device");
/// let decoder = Arc::new(PassthroughDecoder::new(
///     device.backends[0].descriptor.modes[0].format.code,
/// ));
/// let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
///     .decoder(decoder)
///     .start()?;
///
/// while let RecvOutcome::Data(frame) = pipeline.next() {
///     println!("frame {:?}", frame.meta().format);
/// }
/// # Ok::<(), styx::capture_api::CaptureError>(())
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

    /// Record the final output frames to disk using the provided recorder.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn record_output(mut self, recorder: FrameRecorder) -> Self {
        self.output_recorder = Some(recorder);
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

    /// Attach an image hook that can inspect/transform the frame between decode and encode.
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

    /// Attach a compatibility hook that works through `DynamicImage`.
    #[cfg(all(feature = "hooks", feature = "dynamic-image"))]
    pub fn dynamic_hook<F>(self, mut hook: F) -> Self
    where
        F: FnMut(DynamicImage) -> DynamicImage + Send + 'static,
    {
        self.hook(move |img| {
            let ts = img.meta().timestamp;
            match img.into_dynamic_image() {
                Ok(dynamic) => {
                    <FrameLease as FrameLeaseImageExt>::from_dynamic_image(hook(dynamic), ts)
                        .expect("dynamic hook output must convert back into a frame")
                }
                Err(img) => img,
            }
        })
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
        let capture: CaptureHandle = self.capture.start_with_policy(policy)?;
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
        })
    }
}
