//! Unified pipeline that wires capture, decode, hook, and encode in one object.

use std::sync::Arc;
use std::thread;
use std::time::Instant;

#[cfg(feature = "hooks")]
use image::DynamicImage;
#[cfg(feature = "hooks")]
use styx_codec::decoder::frame_to_dynamic_image;
#[cfg(feature = "hooks")]
use styx_codec::image_utils::dynamic_image_to_frame;
use styx_codec::prelude::*;
#[cfg(feature = "hooks")]
type HookFn = Box<dyn FnMut(DynamicImage) -> DynamicImage + Send>;
#[cfg(feature = "hooks")]
type FrameHookFn = Box<dyn FnMut(FrameLease) -> FrameLease + Send>;
#[cfg(feature = "hooks")]
enum HookStore<T> {
    Local(Option<T>),
}

#[cfg(feature = "hooks")]
impl<T> HookStore<T> {
    fn take(&mut self) -> T {
        match self {
            HookStore::Local(h) => h.take().expect("hook missing"),
        }
    }

    fn put(&mut self, hook: T) {
        match self {
            HookStore::Local(h) => {
                *h = Some(hook);
            }
        }
    }
}

use crate::capture_api::{CaptureHandle, CaptureRequest};

#[cfg(feature = "hooks")]
fn apply_image_transform(img: DynamicImage, transform: FrameTransform) -> DynamicImage {
    let rotated = match transform.rotation {
        Rotation90::Deg0 => img,
        Rotation90::Deg90 => img.rotate90(),
        Rotation90::Deg180 => img.rotate180(),
        Rotation90::Deg270 => img.rotate270(),
    };
    if transform.mirror {
        rotated.fliph()
    } else {
        rotated
    }
}


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
    decode_enabled: bool,
    encode_enabled: bool,
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
            decode_enabled: true,
            encode_enabled: true,
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

    /// Attach a decoder by looking it up in the registry.
    ///
    /// # Example
    /// ```rust,ignore
    /// use styx::prelude::*;
    ///
    /// let registry = CodecRegistry::new();
    /// let handle = registry.handle();
    /// let device = probe_all().into_iter().next().expect("device");
    /// let builder = MediaPipelineBuilder::new(CaptureRequest::new(&device))
    ///     .decoder_from_registry(&handle, FourCc::new(*b"MJPG"), None, false)?;
    /// # Ok::<(), styx::codec::RegistryError>(())
    /// ```
    pub fn decoder_from_registry(
        mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<Self, RegistryError> {
        let decoder = lookup_codec(registry, fourcc, impl_name, prefer_hardware)?;
        self.decoder = Some(decoder);
        Ok(self)
    }

    /// Attach an encoder by looking it up in the registry.
    ///
    /// # Example
    /// ```rust,ignore
    /// use styx::prelude::*;
    ///
    /// let registry = CodecRegistry::new();
    /// let handle = registry.handle();
    /// let device = probe_all().into_iter().next().expect("device");
    /// let builder = MediaPipelineBuilder::new(CaptureRequest::new(&device))
    ///     .encoder_from_registry(&handle, FourCc::new(*b"MJPG"), None, false)?;
    /// # Ok::<(), styx::codec::RegistryError>(())
    /// ```
    pub fn encoder_from_registry(
        mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<Self, RegistryError> {
        let encoder = lookup_codec(registry, fourcc, impl_name, prefer_hardware)?;
        self.encoder = Some(encoder);
        Ok(self)
    }

    /// Attach an image hook that can inspect/transform the frame between decode and encode.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn hook<F>(mut self, hook: F) -> Self
    where
        F: FnMut(DynamicImage) -> DynamicImage + Send + 'static,
    {
        self.hook = Some(HookStore::Local(Some(Box::new(hook))));
        self
    }

    /// Attach a frame-level hook that works on `FrameLease` without image conversion.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn frame_hook<F>(mut self, hook: F) -> Self
    where
        F: FnMut(FrameLease) -> FrameLease + Send + 'static,
    {
        self.frame_hook = Some(HookStore::Local(Some(Box::new(hook))));
        self
    }

    /// Apply a fixed frame transform between decode and encode.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn frame_transform(mut self, transform: FrameTransform) -> Self {
        self.frame_transform = transform;
        self
    }

    /// Rotate the stream in 90-degree steps.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn rotate(mut self, rotation: Rotation90) -> Self {
        self.frame_transform.rotation = rotation;
        self
    }

    /// Mirror the stream horizontally.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn mirror(mut self, mirror: bool) -> Self {
        self.frame_transform.mirror = mirror;
        self
    }

    /// Start the pipeline.
    ///
    /// This spins up capture workers and returns a running `MediaPipeline`.
    pub fn start(self) -> Result<MediaPipeline, crate::capture_api::CaptureError> {
        let capture = self.capture.start()?;
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
            metrics: crate::metrics::PipelineMetrics::default(),
            decode_enabled: self.decode_enabled,
            encode_enabled: self.encode_enabled,
        })
    }
}

/// Running pipeline session.
///
/// Use `next` or `next_blocking` to pull frames through the pipeline.
pub struct MediaPipeline {
    capture: CaptureHandle,
    decoder: Option<Arc<dyn Codec>>,
    encoder: Option<Arc<dyn Codec>>,
    #[cfg(feature = "hooks")]
    hook: Option<HookStore<HookFn>>,
    #[cfg(feature = "hooks")]
    frame_hook: Option<HookStore<FrameHookFn>>,
    #[cfg(feature = "hooks")]
    frame_transform: FrameTransform,
    metrics: crate::metrics::PipelineMetrics,
    decode_enabled: bool,
    encode_enabled: bool,
}

impl MediaPipeline {
    /// Process the next frame through decode→hook→encode, returning the final frame.
    ///
    /// Returns `RecvOutcome::Empty` when the capture queue is momentarily empty.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> RecvOutcome<FrameLease> {
        let capture_start = Instant::now();
        match self.capture.recv() {
            RecvOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                self.process_frame(frame)
            }
            RecvOutcome::Empty => {
                // Avoid tight spin when capture is momentarily empty.
                std::thread::yield_now();
                RecvOutcome::Empty
            }
            RecvOutcome::Closed => RecvOutcome::Closed,
        }
    }

    /// Blocking helper that waits between polls to avoid hot spinning.
    ///
    /// Use a small duration (e.g. 2-5ms) to reduce CPU usage.
    pub fn next_blocking(&mut self, wait: std::time::Duration) -> RecvOutcome<FrameLease> {
        loop {
            match self.next() {
                RecvOutcome::Empty => {
                    if !wait.is_zero() {
                        std::thread::sleep(wait);
                    } else {
                        std::thread::yield_now();
                    }
                }
                other => return other,
            }
        }
    }

    #[cfg(feature = "async")]
    /// Async variant of `next`, using the capture async API.
    pub async fn next_async(&mut self) -> RecvOutcome<FrameLease> {
        let capture_start = Instant::now();
        match self.capture.recv_async().await {
            RecvOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                tokio::task::block_in_place(|| self.process_frame(frame))
            }
            RecvOutcome::Empty => {
                // Yield to avoid hot spin when capture queue is momentarily empty.
                tokio::task::yield_now().await;
                RecvOutcome::Empty
            }
            RecvOutcome::Closed => RecvOutcome::Closed,
        }
    }

    /// Spawn an async worker that pumps the pipeline until closed.
    ///
    /// Useful when you want a background task that drains the pipeline.
    #[cfg(feature = "async")]
    pub fn spawn_async_worker(mut self) -> tokio::task::JoinHandle<()> {
        tokio::task::spawn(async move {
            loop {
                match self.next_async().await {
                    RecvOutcome::Data(_) => {}
                    RecvOutcome::Empty => tokio::task::yield_now().await,
                    RecvOutcome::Closed => {
                        self.capture.stop_in_place();
                        break;
                    }
                }
            }
        })
    }

    /// Access the underlying capture handle for control operations.
    pub fn capture(&self) -> &CaptureHandle {
        &self.capture
    }

    /// Swap the decoder at runtime.
    ///
    /// Pass `None` to disable decoding.
    pub fn set_decoder(&mut self, decoder: Option<Arc<dyn Codec>>) {
        self.decoder = decoder;
    }

    /// Swap the decoder using a registry lookup (optionally by impl name).
    ///
    /// Returns an error if no decoder is registered for the requested FourCc.
    pub fn set_decoder_from_registry(
        &mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<(), RegistryError> {
        let decoder = lookup_codec(registry, fourcc, impl_name, prefer_hardware)?;
        self.decoder = Some(decoder);
        Ok(())
    }

    /// Swap the encoder at runtime.
    ///
    /// Pass `None` to disable encoding.
    pub fn set_encoder(&mut self, encoder: Option<Arc<dyn Codec>>) {
        self.encoder = encoder;
    }

    /// Swap the encoder using a registry lookup (optionally by impl name).
    ///
    /// Returns an error if no encoder is registered for the requested FourCc.
    pub fn set_encoder_from_registry(
        &mut self,
        registry: &CodecRegistryHandle,
        fourcc: FourCc,
        impl_name: Option<&str>,
        prefer_hardware: bool,
    ) -> Result<(), RegistryError> {
        let encoder = lookup_codec(registry, fourcc, impl_name, prefer_hardware)?;
        self.encoder = Some(encoder);
        Ok(())
    }

    /// Swap the hook at runtime.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn set_hook<F>(&mut self, hook: Option<F>)
    where
        F: FnMut(DynamicImage) -> DynamicImage + Send + 'static,
    {
        self.hook = hook.map(|h| HookStore::Local(Some(Box::new(h) as HookFn)));
    }

    /// Swap the frame-level hook at runtime.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn set_frame_hook<F>(&mut self, hook: Option<F>)
    where
        F: FnMut(FrameLease) -> FrameLease + Send + 'static,
    {
        self.frame_hook = hook.map(|h| HookStore::Local(Some(Box::new(h) as FrameHookFn)));
    }

    /// Reconfigure capture by stopping the current handle before starting a new one.
    ///
    /// This fully restarts the backend.
    pub fn reconfigure_capture(
        &mut self,
        request: CaptureRequest<'_>,
    ) -> Result<(), crate::capture_api::CaptureError> {
        self.capture.reconfigure_in_place(request)
    }

    /// Stop the pipeline and capture.
    pub fn stop(mut self) {
        self.capture.stop_in_place();
        self.cleanup_pools();
    }

    /// Replace the capture handle, stopping the old one.
    ///
    /// Useful when you want to swap devices without recreating the pipeline.
    pub fn set_capture(&mut self, capture: CaptureHandle) {
        let old = std::mem::replace(&mut self.capture, capture);
        old.stop();
    }

    /// Enable/disable decode stage.
    ///
    /// Disabling decode passes through captured frames directly.
    pub fn enable_decode(&mut self, enabled: bool) {
        self.decode_enabled = enabled;
    }

    /// Enable/disable encode stage.
    ///
    /// Disabling encode returns the post-hook frame.
    pub fn enable_encode(&mut self, enabled: bool) {
        self.encode_enabled = enabled;
    }

    /// Update the frame transform.
    ///
    /// Requires the `hooks` feature.
    #[cfg(feature = "hooks")]
    pub fn set_frame_transform(&mut self, transform: FrameTransform) {
        self.frame_transform = transform;
    }

    /// Access pipeline metrics (capture/decode/encode).
    pub fn metrics(&self) -> crate::metrics::PipelineMetrics {
        self.metrics.clone()
    }

    pub fn memory_stats(&self) -> crate::metrics::PipelineMemoryStats {
        crate::metrics::PipelineMemoryStats {
            transform_pool: styx_core::transform::transform_pool_stats(),
            #[cfg(feature = "hooks")]
            image_pool: styx_codec::image_utils::dynamic_image_pool_stats(),
            #[cfg(feature = "hooks")]
            packed_pools: styx_codec::decoder::packed_frame_pool_stats(),
        }
    }

    /// Spawn a blocking worker that pumps the pipeline until closed.
    ///
    /// This runs on a dedicated thread.
    pub fn spawn_worker(mut self) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            loop {
                match self.next() {
                    RecvOutcome::Data(_) => {}
                    RecvOutcome::Empty => thread::yield_now(),
                    RecvOutcome::Closed => {
                        self.capture.stop_in_place();
                        break;
                    }
                }
            }
        })
    }

    fn process_frame(&mut self, frame: FrameLease) -> RecvOutcome<FrameLease> {
        let mut cur = frame;
        if self.decode_enabled
            && let Some(dec) = &self.decoder
        {
            let t = Instant::now();
            match dec.process(cur) {
                Ok(f) => {
                    self.metrics.decode.record(t.elapsed());
                    cur = f;
                }
                Err(_) => return RecvOutcome::Closed,
            }
        }
        #[cfg(feature = "hooks")]
        if let Some(hook) = &mut self.frame_hook {
            let mut h = hook.take();
            cur = (h)(cur);
            hook.put(h);
        }
        #[cfg(feature = "hooks")]
        {
            let mut transform_applied = false;
            if !self.frame_transform.is_identity() {
                if let Ok(frame) = transform_packed_frame(&cur, self.frame_transform) {
                    cur = frame;
                    transform_applied = true;
                }
            }
            let needs_image = self.hook.is_some()
                || (!self.frame_transform.is_identity() && !transform_applied);
            if needs_image {
                let ts = cur.meta().timestamp;
                match frame_to_dynamic_image(&cur) {
                    Some(mut img) => {
                        if !self.frame_transform.is_identity() && !transform_applied {
                            img = apply_image_transform(img, self.frame_transform);
                        }
                        if let Some(hook) = &mut self.hook {
                            let mut h = hook.take();
                            img = (h)(img);
                            hook.put(h);
                        }
                        if let Some(f) = dynamic_image_to_frame(img, ts) {
                            cur = f;
                        } else {
                            return RecvOutcome::Closed;
                        }
                    }
                    None => return RecvOutcome::Closed,
                }
            }
        }
        if let Some(enc) = &self.encoder
            && self.encode_enabled
        {
            let t = Instant::now();
            match enc.process(cur) {
                Ok(f) => {
                    self.metrics.encode.record(t.elapsed());
                    cur = f;
                }
                Err(_) => return RecvOutcome::Closed,
            }
        }
        RecvOutcome::Data(cur)
    }

    fn cleanup_pools(&self) {
        #[cfg(feature = "hooks")]
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

fn lookup_codec(
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
            match MediaPipeline::next(self) {
                RecvOutcome::Data(f) => return Some(f),
                RecvOutcome::Empty => continue,
                RecvOutcome::Closed => return None,
            }
        }
    }
}
