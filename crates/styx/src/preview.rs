//! Camera previews for user interfaces: small JPEG frames at a capped rate, encoded on a
//! thread of their own, that never get in the way of the camera's real consumer (feature
//! `preview`, Linux; see `docs/preview.md`).
//!
//! ```no_run
//! use styx::ipc::FrameClient;
//! use styx::preview::{Preview, PreviewConfig};
//!
//! // A low-priority client of the camera service: it never changes the vision client's plan,
//! // never restarts its capture, and connects only while someone watches.
//! let preview = Preview::from_service(
//!     FrameClient::options("/run/styx/cameras.sock").camera("front"),
//!     PreviewConfig::new().size(640, 400).max_fps(15.0).quality(70),
//! )?;
//! let mut viewer = preview.subscribe();
//! while let Some(frame) = viewer.recv(std::time::Duration::from_secs(1)) {
//!     // frame.jpeg: one JPEG; frame.sequence, frame.timestamp_ns, frame.width, ...
//! }
//! # Ok::<(), styx::preview::PreviewError>(())
//! ```
//!
//! - **Sources:** a camera service ([`Preview::from_service`], a
//!   [low-priority](crate::ipc::ClientPriority::Low) client), or frames your own code offers
//!   ([`Preview::offer`], e.g. the vision consumer's frames in the same process).
//! - **Encoding:** the frame (or its smallest companion that covers the preview size) is scaled
//!   to the preview size as planar YUV 4:2:0 and encoded as it is (libjpeg-turbo takes the
//!   planes; no colour conversion), on the preview thread, which runs at a lower priority
//!   ([`PreviewConfig::nice`]). Camera JPEG (MJPEG) frames are passed through. The frame is
//!   released as soon as it is scaled, before encoding.
//! - **Latest frame only:** at most one frame waits for the encoder; a newer one replaces it
//!   (counted as `dropped_busy`). Frames beyond [`PreviewConfig::max_fps`] are dropped before
//!   anything is done with them. Viewers ([`PreviewSubscriber`]) get the newest JPEG they have
//!   not seen, never a queue.
//! - **Serving:** [`MjpegStream`] (a `multipart/x-mixed-replace` body as a `Stream` of
//!   `Bytes`, for any HTTP server) and [`ws_message`] (one binary WebSocket message per
//!   frame: a 32-byte header and the JPEG).
//! - **Metrics:** [`Preview::metrics`] and [`crate::metrics::snapshot`]`().previews`: encode and
//!   scale times, bytes per frame, drops by cause, the CPU time of the preview thread.

#[cfg(feature = "preview-window")]
mod window;
#[cfg(feature = "preview-window")]
pub use window::PreviewWindow;

#[cfg(all(feature = "preview", target_os = "linux"))]
mod output;
#[cfg(all(feature = "preview", target_os = "linux"))]
mod scale;
#[cfg(all(feature = "preview", target_os = "linux"))]
mod serve;
#[cfg(all(test, feature = "preview", target_os = "linux"))]
mod tests;
#[cfg(all(feature = "preview", target_os = "linux"))]
mod worker;

#[cfg(all(feature = "preview", target_os = "linux"))]
pub use self::api::*;
#[cfg(all(feature = "preview", target_os = "linux"))]
pub use self::output::{NextPreviewFrame, PreviewSubscriber};
#[cfg(all(feature = "preview", target_os = "linux"))]
pub use self::serve::{
    MJPEG_BOUNDARY, MJPEG_CONTENT_TYPE, MjpegStream, TryMjpegStream, WS_HEADER_LEN, WS_MAGIC,
    WsHeader, parse_ws_message, ws_message,
};
#[cfg(all(feature = "preview", target_os = "linux"))]
pub use styx_codec::jpeg_planar::JpegBackend;

#[cfg(all(feature = "preview", target_os = "linux"))]
mod api {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use bytes::Bytes;
    use parking_lot::{Condvar, Mutex};
    use styx_codec::jpeg_planar::{JpegBackend, PlanarJpegEncoder};
    use styx_core::prelude::*;

    use super::output::{Output, PreviewSubscriber};
    use super::serve::MjpegStream;
    use super::worker::{self, RateGate, Source};
    use crate::ipc::ClientOptions;
    use crate::metrics::{PreviewCounters, PreviewMetrics};
    use crate::planner::{FrameRequest, Frames};

    /// How a preview looks and what it may cost.
    #[derive(Clone, Debug)]
    pub struct PreviewConfig {
        /// Shown in metrics (`styx_preview_*{preview="..."}`).
        pub name: String,
        /// The largest preview: frames are scaled to fit, keeping their aspect ratio, never
        /// up (default 640x400).
        pub max_size: (u32, u32),
        /// Most frames per second encoded (default 15; 0: every frame).
        pub max_fps: f32,
        /// JPEG quality, 1–100 (default 70).
        pub quality: u8,
        /// The JPEG encoder (default [`JpegBackend::Auto`]: libjpeg-turbo when compiled in).
        pub encoder: JpegBackend,
        /// Niceness of the preview thread (default `Some(10)`: below the camera's threads;
        /// `None`: unchanged). Raising priority (negative) needs privileges and is ignored
        /// when refused.
        pub nice: Option<i32>,
        /// Encode while nobody is subscribed (default false: frames are dropped unencoded,
        /// and a service client disconnects after [`PreviewConfig::idle_disconnect`]).
        pub encode_unwatched: bool,
        /// Pass camera JPEG (MJPEG) frames through as they are, without scaling (default
        /// true; otherwise they are refused: decode them with the request).
        pub passthrough_jpeg: bool,
        /// How long a camera service client stays connected without subscribers (default
        /// 5 s), so a viewer reloading the page does not reconnect.
        pub idle_disconnect: Duration,
        /// What a camera service client asks for (default: NV12 at `max_size`; see
        /// [`PreviewConfig::request`]).
        pub request: Option<FrameRequest>,
    }

    impl Default for PreviewConfig {
        fn default() -> Self {
            Self {
                name: "preview".into(),
                max_size: (640, 400),
                max_fps: 15.0,
                quality: 70,
                encoder: JpegBackend::Auto,
                nice: Some(10),
                encode_unwatched: false,
                passthrough_jpeg: true,
                idle_disconnect: Duration::from_secs(5),
                request: None,
            }
        }
    }

    impl PreviewConfig {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn name(mut self, name: impl Into<String>) -> Self {
            self.name = name.into();
            self
        }

        /// The largest preview (frames scaled to fit, aspect ratio kept).
        pub fn size(mut self, width: u32, height: u32) -> Self {
            self.max_size = (width.max(2), height.max(2));
            self
        }

        /// Most frames per second (0: every frame).
        pub fn max_fps(mut self, fps: f32) -> Self {
            self.max_fps = if fps.is_finite() { fps.max(0.0) } else { 0.0 };
            self
        }

        pub fn quality(mut self, quality: u8) -> Self {
            self.quality = quality.clamp(1, 100);
            self
        }

        pub fn encoder(mut self, encoder: JpegBackend) -> Self {
            self.encoder = encoder;
            self
        }

        /// Niceness of the preview thread (`None`: unchanged).
        pub fn nice(mut self, nice: Option<i32>) -> Self {
            self.nice = nice;
            self
        }

        pub fn encode_unwatched(mut self, encode: bool) -> Self {
            self.encode_unwatched = encode;
            self
        }

        pub fn passthrough_jpeg(mut self, passthrough: bool) -> Self {
            self.passthrough_jpeg = passthrough;
            self
        }

        pub fn idle_disconnect(mut self, after: Duration) -> Self {
            self.idle_disconnect = after;
            self
        }

        /// Ask the camera service for these frames instead of NV12 at the preview size. Any
        /// format the preview scales works; it is asked at low priority, so the service may
        /// give a share of another client's frames instead.
        pub fn with_request(mut self, request: FrameRequest) -> Self {
            self.request = Some(request);
            self
        }

        /// What a camera service client asks for: [`PreviewConfig::with_request`]'s, else
        /// NV12 at the preview size (the ISP's second output where the capture has it free).
        pub fn request(&self) -> FrameRequest {
            self.request
                .clone()
                .unwrap_or_else(|| Frames::nv12().size(self.max_size.0, self.max_size.1))
        }
    }

    /// One encoded preview frame.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct PreviewFrame {
        /// The JPEG file (shared between viewers without copying).
        pub jpeg: Bytes,
        /// 1, 2, ... for each frame this preview published; gaps are frames a viewer skipped.
        pub sequence: u64,
        /// The camera frame's timestamp (`FrameMeta::timestamp`, nanoseconds) on `clock`.
        pub timestamp_ns: u64,
        pub clock: Option<TimestampClock>,
        pub width: u32,
        pub height: u32,
        /// A grey JPEG (from a luma-only source).
        pub gray: bool,
        /// Camera JPEG passed through as it was.
        pub passthrough: bool,
        /// Time to scale and encode it, microseconds.
        pub encode_us: u32,
    }

    #[derive(Debug, thiserror::Error)]
    pub enum PreviewError {
        #[error("preview encoder: {0}")]
        Encoder(#[from] styx_codec::CodecError),
        #[error("preview thread: {0}")]
        Thread(#[from] std::io::Error),
    }

    pub(super) struct Shared {
        pub(super) config: PreviewConfig,
        pub(super) output: Arc<Output>,
        /// The frame waiting for the encoder (offered frames).
        pub(super) input: Mutex<Option<FrameLease>>,
        pub(super) input_ready: Condvar,
        pub(super) gate: Mutex<RateGate>,
        pub(super) stop: AtomicBool,
        pub(super) counters: Arc<PreviewCounters>,
    }

    impl Shared {
        pub(super) fn watched(&self) -> bool {
            self.config.encode_unwatched || self.output.subscribers.load(Ordering::Acquire) > 0
        }
    }

    /// A running preview: frames in, JPEG out on its own thread. Stops when dropped.
    pub struct Preview {
        shared: Arc<Shared>,
        thread: Option<JoinHandle<()>>,
    }

    impl std::fmt::Debug for Preview {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Preview")
                .field("config", &self.shared.config)
                .finish()
        }
    }

    impl Preview {
        /// A preview of the frames you [offer](Preview::offer) it.
        pub fn new(config: PreviewConfig) -> Result<Self, PreviewError> {
            Self::start(config, Source::Offered)
        }

        /// A preview of a camera service's camera: a [low-priority](ClientOptions::low_priority),
        /// reconnecting client of the service at `options` (its camera and timeout), connected
        /// only while someone watches (see [`PreviewConfig::encode_unwatched`] and
        /// [`PreviewConfig::idle_disconnect`]). It never blocks: it connects in the background,
        /// so the service need not be up yet.
        ///
        /// The service plans it after its normal clients: it gets the frames it asks for
        /// ([`PreviewConfig::request`]) when that changes nothing for them (the ISP's free
        /// second output when the capture starts with it), else a share of a normal client's
        /// frames, which the preview scales itself; it never restarts their capture.
        pub fn from_service(
            options: ClientOptions,
            config: PreviewConfig,
        ) -> Result<Self, PreviewError> {
            Self::start(
                config,
                Source::Service(options.low_priority().reconnecting()),
            )
        }

        fn start(config: PreviewConfig, source: Source) -> Result<Self, PreviewError> {
            let encoder = PlanarJpegEncoder::new(config.encoder, config.quality)?;
            let counters =
                PreviewCounters::new(config.name.clone(), encoder.backend().name().into());
            let shared = Arc::new(Shared {
                gate: Mutex::new(RateGate::new(config.max_fps)),
                config,
                output: Arc::default(),
                input: Mutex::new(None),
                input_ready: Condvar::new(),
                stop: AtomicBool::new(false),
                counters,
            });
            let thread = {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name("styx-preview".into())
                    .spawn(move || worker::run(&shared, source, encoder))?
            };
            Ok(Self {
                shared,
                thread: Some(thread),
            })
        }

        /// Offer a frame (without waiting, and without copying it: the preview keeps a share
        /// of it until it has scaled it). Returns whether the preview took it: not when
        /// nobody watches, the frame rate cap says it is too soon, or the frame is not
        /// shareable (it owns its buffers: `FrameLease::into_shareable` makes it shareable
        /// without copying). A frame still waiting for the encoder is replaced.
        pub fn offer(&self, frame: &FrameLease) -> bool {
            let counters = &self.shared.counters;
            counters.frames_in.fetch_add(1, Ordering::Relaxed);
            if !self.shared.watched() {
                counters.dropped_unwatched.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            if !self.shared.gate.lock().due(std::time::Instant::now()) {
                counters.dropped_rate.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            let Some(share) = frame.share() else {
                counters
                    .error("offered frame is not shareable (FrameLease::into_shareable)".into());
                return false;
            };
            self.put(share);
            true
        }

        /// [`Preview::offer`] for a frame you no longer need.
        pub fn offer_owned(&self, frame: FrameLease) -> bool {
            let counters = &self.shared.counters;
            counters.frames_in.fetch_add(1, Ordering::Relaxed);
            if !self.shared.watched() {
                counters.dropped_unwatched.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            if !self.shared.gate.lock().due(std::time::Instant::now()) {
                counters.dropped_rate.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            self.put(frame);
            true
        }

        fn put(&self, frame: FrameLease) {
            let replaced = self.shared.input.lock().replace(frame);
            if replaced.is_some() {
                self.shared
                    .counters
                    .dropped_busy
                    .fetch_add(1, Ordering::Relaxed);
            }
            self.shared.input_ready.notify_one();
            // The replaced frame (its camera buffer) goes here, outside the lock.
            drop(replaced);
        }

        /// A new viewer: the newest frame it has not seen, each time it asks.
        pub fn subscribe(&self) -> PreviewSubscriber {
            self.shared.output.subscribe()
        }

        /// A `multipart/x-mixed-replace` MJPEG body for a new viewer (see [`MjpegStream`]).
        pub fn mjpeg(&self) -> MjpegStream {
            MjpegStream::new(self.subscribe())
        }

        /// The latest frame, if any (without subscribing).
        pub fn latest(&self) -> Option<PreviewFrame> {
            self.shared.output.latest()
        }

        pub fn metrics(&self) -> PreviewMetrics {
            self.shared.counters.snapshot()
        }

        pub fn config(&self) -> &PreviewConfig {
            &self.shared.config
        }

        /// Stop the thread (subscribers end after the latest frame).
        pub fn stop(mut self) {
            self.shut_down();
        }

        fn shut_down(&mut self) {
            self.shared.stop.store(true, Ordering::Release);
            self.shared.input_ready.notify_all();
            self.shared.output.close();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for Preview {
        fn drop(&mut self) {
            self.shut_down();
        }
    }
}
