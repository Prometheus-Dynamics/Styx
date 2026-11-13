//! Camera previews for user interfaces.
//!
//! A [`Preview`] is a consumer of a camera's frames, the way an application's vision code is. It
//! takes a share of a frame (no copy, no allocation), keeps the newest one only, encodes it to a
//! small JPEG with `styx-jpeg` on its own thread, and publishes the JPEG frames as its own shared
//! ring with `styx-socket`: each reader gets the ring's buffers once, over a Unix socket, then one
//! ring entry per JPEG (one plane, the JPEG's length).
//!
//! What the preview does to the camera's consumers:
//!
//! - **Its call costs the reader a few instructions and a lock.** [`Preview::offer`] checks the
//!   rate cap on the frame's timestamp, then swaps a share into a one-frame slot. The JPEG is made
//!   on the preview's thread, never on the caller's.
//! - **Frames from a shared ring keep their hold.** A reader of `styx-socket` must keep its
//!   `Frame` (its hold marks) for as long as the preview holds the frame, or the producer reuses
//!   the buffer under the preview's share and the reader's next frame is malformed. Use
//!   [`Preview::offer_held`] with the `Frame` itself; in process, [`Preview::offer`].
//! - **Rate first.** Frames over `max_fps` are dropped on arrival, evenly spread. Nothing else is
//!   done with them.
//! - **Latest frame only.** A frame that is still waiting when a newer one arrives is replaced
//!   and counted ([`PreviewStats::replaced`]). The share it held is released at once.
//! - **It never waits and never restarts anything.** A preview with no free output buffer drops
//!   the frame and counts it. The queued frame and the one being encoded keep source buffers, so
//!   the source needs spare buffers beyond its own in-flight frames.
//!
//! Serving to a browser (MJPEG, WebSocket) is the application's job: a reader of the ring gets the
//! JPEG bytes in place.
//!
//! Linux only: the output ring is a Linux shared-memory layout (`styx-socket`).

#![cfg(target_os = "linux")]

mod encode;
mod rate;
mod worker;

use std::fmt;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use styx_frame::FrameLease;
use styx_jpeg::JpegEncoder;
use styx_socket::{Producer, ProducerConfig, SocketError};

use rate::RateCap;

/// What a preview is made with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewConfig {
    /// The largest source frame offered, in pixels. The JPEG buffers are sized for it, so
    /// larger frames are refused.
    pub max_width: u32,
    /// See [`Self::max_width`].
    pub max_height: u32,
    /// Frames per second the preview encodes at most. Default 15.
    pub max_fps: f64,
    /// JPEG quality, 1 to 100. Default 70.
    pub quality: u8,
    /// Buffers in the output ring (one memfd each). Default 4.
    pub buffers: usize,
    /// Ring entries kept for readers. A reader further behind than this loses frames. Default 8.
    pub history: u32,
}

impl PreviewConfig {
    /// A preview of frames up to `max_width` by `max_height`, with the defaults.
    pub const fn new(max_width: u32, max_height: u32) -> Self {
        Self {
            max_width,
            max_height,
            max_fps: 15.0,
            quality: 70,
            buffers: 4,
            history: 8,
        }
    }

    /// Caps the encoded frame rate.
    #[must_use]
    pub const fn max_fps(mut self, fps: f64) -> Self {
        self.max_fps = fps;
        self
    }

    /// Sets the JPEG quality.
    #[must_use]
    pub const fn quality(mut self, quality: u8) -> Self {
        self.quality = quality;
        self
    }

    /// Sets the number of output buffers.
    #[must_use]
    pub const fn buffers(mut self, buffers: usize) -> Self {
        self.buffers = buffers;
        self
    }

    /// Sets the number of ring entries kept.
    #[must_use]
    pub const fn history(mut self, history: u32) -> Self {
        self.history = history;
        self
    }
}

/// Why a preview could not be made.
#[derive(Debug)]
pub enum PreviewError {
    /// The configuration is not usable (the reason says which value).
    Config(&'static str),
    /// The output ring could not be made.
    Ring(SocketError),
    /// The worker thread could not start.
    Thread(std::io::Error),
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(why) => write!(f, "preview configuration: {why}"),
            Self::Ring(err) => write!(f, "preview ring: {err}"),
            Self::Thread(err) => write!(f, "preview thread: {err}"),
        }
    }
}

impl std::error::Error for PreviewError {}

/// What [`Preview::offer`] did with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// The frame is the newest waiting for the encoder.
    Queued,
    /// Over the rate cap: dropped on arrival.
    RateCapped,
    /// Not a frame this preview can encode, or larger than its configured size.
    Refused,
}

/// Counts for a preview, for reporting. Every offered frame is counted once in `offered` and
/// once in one of the outcomes below it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewStats {
    /// Frames offered.
    pub offered: u64,
    /// Frames dropped by the rate cap.
    pub rate_capped: u64,
    /// Frames refused (format, size).
    pub refused: u64,
    /// Queued frames replaced by a newer one before the encoder took them.
    pub replaced: u64,
    /// JPEG frames published.
    pub published: u64,
    /// Frames dropped because no output buffer was free.
    pub no_buffer: u64,
    /// Frames that failed to encode or publish.
    pub failed: u64,
    /// JPEG bytes published.
    pub jpeg_bytes: u64,
    /// Encode time summed over published frames, in nanoseconds.
    pub encode_ns: u64,
    /// Readers connected to the output ring now.
    pub readers: usize,
}

/// The state the offering thread and the worker share.
#[derive(Debug)]
struct Shared<M, K> {
    state: Mutex<State<M, K>>,
    wake: Condvar,
}

#[derive(Debug)]
struct State<M, K> {
    /// The newest frame not yet taken by the worker, with what keeps it alive.
    latest: Option<(FrameLease<M>, K)>,
    /// Readers handed to the worker, to be accepted between frames.
    attach: Vec<UnixStream>,
    cap: RateCap,
    max_width: u32,
    max_height: u32,
    closing: bool,
    stats: PreviewStats,
}

/// A camera preview: offer frames, attach readers, read JPEG frames from the ring.
///
/// Dropping it stops the worker thread and closes the output ring for its readers.
///
/// `K` is what a frame must keep alive while the preview holds it: `()` for in-process frames,
/// the reader's `styx_socket::Frame` for frames read from a shared ring.
#[derive(Debug)]
pub struct Preview<M: Send + Sync + 'static, K: Send + 'static = ()> {
    shared: Arc<Shared<M, K>>,
    worker: Option<JoinHandle<()>>,
}

impl<M: Send + Sync + 'static, K: Send + 'static> Preview<M, K> {
    /// A preview with `config`, its output ring made and its worker thread started.
    ///
    /// # Errors
    ///
    /// [`PreviewError::Config`] for a value out of range, [`PreviewError::Ring`] if the ring can't
    /// be made, [`PreviewError::Thread`] if the thread can't start.
    pub fn new(config: PreviewConfig) -> Result<Self, PreviewError> {
        if config.max_width == 0 || config.max_height == 0 {
            return Err(PreviewError::Config("the largest frame has no pixels"));
        }
        if !(config.max_fps.is_finite() && config.max_fps > 0.0) {
            return Err(PreviewError::Config(
                "max_fps must be finite and above zero",
            ));
        }
        if !(1..=100).contains(&config.quality) {
            return Err(PreviewError::Config("quality is 1 to 100"));
        }
        let capacity = jpeg_capacity(config.max_width, config.max_height);
        let producer = Producer::new(ProducerConfig {
            payload: capacity,
            buffers: config.buffers,
            history: config.history,
        })
        .map_err(PreviewError::Ring)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                latest: None,
                attach: Vec::new(),
                cap: RateCap::new(config.max_fps),
                max_width: config.max_width,
                max_height: config.max_height,
                closing: false,
                stats: PreviewStats::default(),
            }),
            wake: Condvar::new(),
        });
        let worker = thread::Builder::new()
            .name("styx-preview".into())
            .spawn({
                let shared = Arc::clone(&shared);
                let encoder = JpegEncoder::new(config.quality);
                move || worker::run(&shared, producer, encoder, capacity)
            })
            .map_err(PreviewError::Thread)?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Offers a frame, keeping `keep` alive for as long as the preview holds the frame. Returns at
    /// once: the frame's rate cap is checked, and a share of the frame (no copy) becomes the newest
    /// one waiting, replacing any older one (and its `keep`).
    pub fn offer_held(&self, keep: K, frame: &FrameLease<M>) -> Offer {
        let mut state = lock(&self.shared.state);
        state.stats.offered += 1;
        let format = frame.format();
        let fits = format.resolution.width.get() <= state.max_width
            && format.resolution.height.get() <= state.max_height;
        if !fits || !styx_jpeg::INPUTS.contains(&format.code) {
            state.stats.refused += 1;
            return Offer::Refused;
        }
        if !state.cap.admit(frame.timestamp_ns()) {
            state.stats.rate_capped += 1;
            return Offer::RateCapped;
        }
        if state.latest.replace((frame.share(), keep)).is_some() {
            state.stats.replaced += 1;
        }
        drop(state);
        self.shared.wake.notify_one();
        Offer::Queued
    }

    /// Hands a reader's socket to the output ring. The worker runs the handshake between frames
    /// (the reader must be connecting on its end, see `styx_socket::Reader::connect`); a reader
    /// that does not answer holds the worker for up to five seconds.
    pub fn attach(&self, socket: UnixStream) {
        lock(&self.shared.state).attach.push(socket);
        self.shared.wake.notify_one();
    }

    /// Counts, now.
    pub fn stats(&self) -> PreviewStats {
        lock(&self.shared.state).stats
    }

    /// The interval the rate cap enforces, in nanoseconds.
    pub fn interval_ns(&self) -> u64 {
        lock(&self.shared.state).cap.interval_ns()
    }
}

impl<M: Send + Sync + 'static> Preview<M, ()> {
    /// Offers a frame that needs nothing kept alive beyond the frame itself (see
    /// [`Preview::offer_held`]).
    pub fn offer(&self, frame: &FrameLease<M>) -> Offer {
        self.offer_held((), frame)
    }
}

impl<M: Send + Sync + 'static, K: Send + 'static> Drop for Preview<M, K> {
    fn drop(&mut self) {
        lock(&self.shared.state).closing = true;
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The payload of one output buffer for frames up to `width` by `height`: three bytes per pixel
/// covers a JPEG at any quality with room to spare.
fn jpeg_capacity(width: u32, height: u32) -> usize {
    (width as usize * height as usize * 3).max(1) + 4096
}

fn lock<M, K>(state: &Mutex<State<M, K>>) -> MutexGuard<'_, State<M, K>> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
