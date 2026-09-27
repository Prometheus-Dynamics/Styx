//! Capture queue, buffer, clock and reconnect settings shared by all backends.

use styx_core::prelude::{ClockSource, QueueOverflow};

use super::{
    DEFAULT_CAPTURE_EXTRA_BUFFERS, DEFAULT_CAPTURE_IDLE_POLL_MS,
    DEFAULT_CAPTURE_QUEUE_SEND_TIMEOUT_MS, DEFAULT_POOL_BYTES, DEFAULT_POOL_MIN,
    DEFAULT_POOL_SPARE, DEFAULT_QUEUE_DEPTH,
};

/// Tunables for capture queues and buffer pools.
///
/// Prefer `StyxConfig` builder methods for application configuration. If direct
/// struct construction is needed, include `..CaptureTunables::default()` so new
/// release tunables pick up their documented defaults.
///
/// # Example
/// ```rust
/// use styx::prelude::*;
///
/// let config = StyxConfig::new()
///     .capture_queue_depth(8)
///     .capture_pool(6, 2 << 20, 8);
/// ```
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct CaptureConfig {
    /// Number of frames buffered between a capture worker and consumers.
    pub queue_depth: usize,
    /// Minimum number of reusable capture buffers to keep in backend pools.
    pub pool_min: usize,
    /// Minimum byte size for each reusable capture buffer.
    pub pool_bytes: usize,
    /// Extra reusable buffers beyond `pool_min` for bursty pipelines.
    pub pool_spare: usize,
    /// Maximum time a generic capture worker waits when enqueueing a frame, in milliseconds.
    pub queue_send_timeout_ms: u64,
    /// Stop polling interval used while a generic capture worker is idle, in milliseconds.
    pub idle_poll_ms: u64,
    /// What happens when a frame arrives while the queue is full. `DropOldest` (default) keeps
    /// the newest frames; `Backpressure` waits `queue_send_timeout_ms`, then drops the new frame.
    pub queue_overflow: QueueOverflow,
    /// Device buffers (libcamera, V4L2) allocated beyond `queue_depth`.
    pub extra_buffers: usize,
    /// Recovery for libcamera and V4L2 cameras that disconnect or stop delivering frames.
    pub reconnect: ReconnectPolicy,
    /// Stop streaming a libcamera or V4L2 camera nobody has pulled a frame from for this long,
    /// freeing its buffers and letting the sensor idle; the next pull starts it again on the same
    /// handle, with its controls (it takes as long as starting the camera). `0` (default)
    /// keeps it streaming. Needs `reconnect` enabled, which runs the capture supervisor.
    pub stop_when_idle_ms: u64,
    /// What stopping an idle camera does (see [`IdleStop`]).
    pub idle_stop: IdleStop,
    /// Clock live sources stamp frames with. File and simulation sources always report media
    /// time (`TimestampClock::StreamRelative`).
    pub timestamp_clock: ClockSource,
}

pub type CaptureTunables = CaptureConfig;

/// How [`CaptureConfig::stop_when_idle_ms`] stops an idle camera.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum IdleStop {
    /// Stop the capture and release the camera and its buffers. Starting again takes as long
    /// as starting the camera (~0.6 s for a USB webcam, ~1.4 s for a Raspberry Pi CSI camera).
    #[default]
    Release,
    /// Stop streaming but keep the camera configured, with its buffers: starting again is much
    /// faster, and the buffers stay allocated. libcamera only; other backends release.
    Pause,
}

/// How a libcamera or V4L2 capture recovers when its camera disconnects or stalls.
///
/// While enabled, the capture handle stays open. Styx re-probes, finds the same camera by its
/// identity keys (its `/dev/video` path may change), and restarts it with the same mode,
/// interval, config and controls. Frames resume on the same handle. Reconnects are reported in
/// `HealthReport::capture_retries`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct ReconnectPolicy {
    pub enabled: bool,
    /// Restart when no frame has been produced for this long while the consumer holds no
    /// frames. It is raised to at least four frame intervals. `0` disables stall detection;
    /// disconnects the backend reports (V4L2 `ENODEV`) still trigger a reconnect.
    pub stall_timeout_ms: u64,
    /// First delay between reconnect attempts; doubles up to `max_backoff_ms`.
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            stall_timeout_ms: 2_000,
            // Re-probing costs under 1 ms once warm (CM5), so retry often.
            initial_backoff_ms: 100,
            max_backoff_ms: 1_000,
        }
    }
}

impl ReconnectPolicy {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PoolLimits {
    pub min: usize,
    pub bytes: usize,
    pub spare: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            queue_depth: DEFAULT_QUEUE_DEPTH,
            pool_min: DEFAULT_POOL_MIN,
            pool_bytes: DEFAULT_POOL_BYTES,
            pool_spare: DEFAULT_POOL_SPARE,
            queue_send_timeout_ms: DEFAULT_CAPTURE_QUEUE_SEND_TIMEOUT_MS,
            idle_poll_ms: DEFAULT_CAPTURE_IDLE_POLL_MS,
            queue_overflow: QueueOverflow::DropOldest,
            extra_buffers: DEFAULT_CAPTURE_EXTRA_BUFFERS,
            reconnect: ReconnectPolicy::default(),
            stop_when_idle_ms: 0,
            idle_stop: IdleStop::Release,
            timestamp_clock: ClockSource::Native,
        }
    }
}

impl CaptureConfig {
    pub(crate) fn sanitized(self) -> Self {
        Self {
            queue_depth: self.queue_depth.max(1),
            pool_min: self.pool_min.max(1),
            pool_bytes: self.pool_bytes.max(1),
            pool_spare: self.pool_spare,
            queue_send_timeout_ms: self.queue_send_timeout_ms.max(1),
            idle_poll_ms: self.idle_poll_ms.max(1),
            queue_overflow: self.queue_overflow,
            extra_buffers: self.extra_buffers,
            reconnect: self.reconnect,
            stop_when_idle_ms: self.stop_when_idle_ms,
            idle_stop: self.idle_stop,
            timestamp_clock: self.timestamp_clock,
        }
    }

    pub(crate) fn pool_limits(
        self,
        default_min: usize,
        default_bytes: usize,
        default_spare: usize,
    ) -> PoolLimits {
        let tunables = self.sanitized();
        PoolLimits {
            min: tunables.pool_min.max(default_min.max(1)),
            bytes: tunables.pool_bytes.max(default_bytes.max(1)),
            spare: tunables.pool_spare.max(default_spare),
        }
    }
}
