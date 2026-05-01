use styx_core::transform::TransformPoolConfig;

/// Default capture queue depth (frames).
pub const DEFAULT_QUEUE_DEPTH: usize = 4;
/// Default buffer pool minimum count.
pub const DEFAULT_POOL_MIN: usize = 4;
/// Default buffer pool bytes per buffer.
pub const DEFAULT_POOL_BYTES: usize = 1 << 20;
/// Default extra spare buffers beyond the minimum.
pub const DEFAULT_POOL_SPARE: usize = 8;
/// Default frame enqueue timeout for generic capture workers (milliseconds).
pub const DEFAULT_CAPTURE_QUEUE_SEND_TIMEOUT_MS: u64 = 10;
/// Default V4L2 mmap dequeue poll timeout (milliseconds).
pub const DEFAULT_V4L2_MMAP_POLL_MS: u64 = 50;
/// Default V4L2 frame enqueue timeout (milliseconds).
pub const DEFAULT_V4L2_SEND_TIMEOUT_MS: u64 = 10;
/// Default V4L2 worker sleep after non-timeout dequeue errors (milliseconds).
pub const DEFAULT_V4L2_ERROR_BACKOFF_MS: u64 = 5;
/// Default libcamera camera lookup timeout (milliseconds).
pub const DEFAULT_LIBCAMERA_LOOKUP_TIMEOUT_MS: u64 = 3_000;
/// Default libcamera camera lookup retry interval (milliseconds).
pub const DEFAULT_LIBCAMERA_LOOKUP_POLL_MS: u64 = 100;
/// Default libcamera request requeue stall timeout (milliseconds).
pub const DEFAULT_LIBCAMERA_REQUEUE_STALL_TIMEOUT_MS: u64 = 2_000;
/// Default libcamera completed-request poll timeout (milliseconds).
pub const DEFAULT_LIBCAMERA_REQUEST_POLL_MS: u64 = 20;
/// Default libcamera idle-stop backing drain timeout (milliseconds).
pub const DEFAULT_LIBCAMERA_IDLE_DRAIN_TIMEOUT_MS: u64 = 2_000;
/// Default libcamera idle-stop backing drain poll interval (milliseconds).
pub const DEFAULT_LIBCAMERA_IDLE_DRAIN_POLL_MS: u64 = 10;
/// Default libcamera control response timeout (milliseconds).
pub const DEFAULT_LIBCAMERA_CONTROL_RESPONSE_TIMEOUT_MS: u64 = 500;
/// Default libcamera probe cache time-to-live (milliseconds).
pub const DEFAULT_LIBCAMERA_PROBE_CACHE_MS: u64 = 1_000;
/// Default libcamera manager idle-stop behavior.
pub const DEFAULT_LIBCAMERA_STOP_WHEN_IDLE: bool = false;
/// Default libcamera request-pool prefault behavior.
pub const DEFAULT_LIBCAMERA_PREFAULT_REQUEST_POOLS: bool = true;
/// Default netcam request timeout (seconds).
pub const DEFAULT_NETCAM_TIMEOUT_SECS: u64 = 5;
/// Default netcam TCP/TLS connect timeout (milliseconds).
pub const DEFAULT_NETCAM_CONNECT_TIMEOUT_MS: u64 = 1_000;
/// Default netcam read timeout between received chunks (milliseconds).
pub const DEFAULT_NETCAM_READ_TIMEOUT_MS: u64 = 2_000;
/// Default netcam backoff start delay (milliseconds).
pub const DEFAULT_NETCAM_BACKOFF_START_MS: u64 = 1_000;
/// Default netcam backoff maximum delay (milliseconds).
pub const DEFAULT_NETCAM_BACKOFF_MAX_MS: u64 = 10_000;
/// Default netcam frame enqueue timeout (milliseconds).
pub const DEFAULT_NETCAM_SEND_TIMEOUT_MS: u64 = 10;
/// Default netcam stop polling interval (milliseconds).
pub const DEFAULT_NETCAM_STOP_POLL_MS: u64 = 50;
/// Default file-backend decoded image cache limit (bytes).
pub const DEFAULT_FILE_IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Preferred libcamera stream role for processed, non-raw/non-encoded requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum LibcameraProcessedStreamRole {
    #[default]
    ViewFinder,
    VideoRecording,
    StillCapture,
}

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
pub struct CaptureTunables {
    pub queue_depth: usize,
    pub pool_min: usize,
    pub pool_bytes: usize,
    pub pool_spare: usize,
    pub queue_send_timeout_ms: u64,
    pub v4l2_mmap_poll_ms: u64,
    pub v4l2_send_timeout_ms: u64,
    pub v4l2_error_backoff_ms: u64,
    pub libcamera_lookup_timeout_ms: u64,
    pub libcamera_lookup_poll_ms: u64,
    pub libcamera_requeue_stall_timeout_ms: u64,
    pub libcamera_request_poll_ms: u64,
    pub libcamera_idle_drain_timeout_ms: u64,
    pub libcamera_idle_drain_poll_ms: u64,
    pub libcamera_control_response_timeout_ms: u64,
    pub libcamera_probe_cache_ttl_ms: u64,
    pub libcamera_stop_when_idle: bool,
    pub libcamera_prefault_request_pools: bool,
    pub libcamera_processed_stream_role: LibcameraProcessedStreamRole,
    pub file_image_cache_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PoolLimits {
    pub min: usize,
    pub bytes: usize,
    pub spare: usize,
}

impl Default for CaptureTunables {
    fn default() -> Self {
        Self {
            queue_depth: DEFAULT_QUEUE_DEPTH,
            pool_min: DEFAULT_POOL_MIN,
            pool_bytes: DEFAULT_POOL_BYTES,
            pool_spare: DEFAULT_POOL_SPARE,
            queue_send_timeout_ms: DEFAULT_CAPTURE_QUEUE_SEND_TIMEOUT_MS,
            v4l2_mmap_poll_ms: DEFAULT_V4L2_MMAP_POLL_MS,
            v4l2_send_timeout_ms: DEFAULT_V4L2_SEND_TIMEOUT_MS,
            v4l2_error_backoff_ms: DEFAULT_V4L2_ERROR_BACKOFF_MS,
            libcamera_lookup_timeout_ms: DEFAULT_LIBCAMERA_LOOKUP_TIMEOUT_MS,
            libcamera_lookup_poll_ms: DEFAULT_LIBCAMERA_LOOKUP_POLL_MS,
            libcamera_requeue_stall_timeout_ms: DEFAULT_LIBCAMERA_REQUEUE_STALL_TIMEOUT_MS,
            libcamera_request_poll_ms: DEFAULT_LIBCAMERA_REQUEST_POLL_MS,
            libcamera_idle_drain_timeout_ms: DEFAULT_LIBCAMERA_IDLE_DRAIN_TIMEOUT_MS,
            libcamera_idle_drain_poll_ms: DEFAULT_LIBCAMERA_IDLE_DRAIN_POLL_MS,
            libcamera_control_response_timeout_ms: DEFAULT_LIBCAMERA_CONTROL_RESPONSE_TIMEOUT_MS,
            libcamera_probe_cache_ttl_ms: DEFAULT_LIBCAMERA_PROBE_CACHE_MS,
            libcamera_stop_when_idle: DEFAULT_LIBCAMERA_STOP_WHEN_IDLE,
            libcamera_prefault_request_pools: DEFAULT_LIBCAMERA_PREFAULT_REQUEST_POOLS,
            libcamera_processed_stream_role: LibcameraProcessedStreamRole::default(),
            file_image_cache_bytes: DEFAULT_FILE_IMAGE_CACHE_BYTES,
        }
    }
}

impl CaptureTunables {
    pub(crate) fn sanitized(self) -> Self {
        Self {
            queue_depth: self.queue_depth.max(1),
            pool_min: self.pool_min.max(1),
            pool_bytes: self.pool_bytes.max(1),
            pool_spare: self.pool_spare,
            queue_send_timeout_ms: self.queue_send_timeout_ms.max(1),
            v4l2_mmap_poll_ms: self.v4l2_mmap_poll_ms.max(1),
            v4l2_send_timeout_ms: self.v4l2_send_timeout_ms.max(1),
            v4l2_error_backoff_ms: self.v4l2_error_backoff_ms.max(1),
            libcamera_lookup_timeout_ms: self.libcamera_lookup_timeout_ms.max(1),
            libcamera_lookup_poll_ms: self.libcamera_lookup_poll_ms.max(1),
            libcamera_requeue_stall_timeout_ms: self.libcamera_requeue_stall_timeout_ms.max(1),
            libcamera_request_poll_ms: self.libcamera_request_poll_ms.max(1),
            libcamera_idle_drain_timeout_ms: self.libcamera_idle_drain_timeout_ms.max(1),
            libcamera_idle_drain_poll_ms: self.libcamera_idle_drain_poll_ms.max(1),
            libcamera_control_response_timeout_ms: self
                .libcamera_control_response_timeout_ms
                .max(1),
            libcamera_probe_cache_ttl_ms: self.libcamera_probe_cache_ttl_ms,
            libcamera_stop_when_idle: self.libcamera_stop_when_idle,
            libcamera_prefault_request_pools: self.libcamera_prefault_request_pools,
            libcamera_processed_stream_role: self.libcamera_processed_stream_role,
            file_image_cache_bytes: self.file_image_cache_bytes,
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

/// Tunables for netcam polling/backoff behavior.
///
/// Prefer `StyxConfig::netcam_*` builder methods for application configuration.
/// If direct struct construction is needed, include `..NetcamTunables::default()`
/// so new release tunables pick up their documented defaults.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct NetcamTunables {
    pub request_timeout_secs: u64,
    pub connect_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub backoff_start_ms: u64,
    pub backoff_max_ms: u64,
    pub send_timeout_ms: u64,
    pub stop_poll_ms: u64,
}

impl Default for NetcamTunables {
    fn default() -> Self {
        Self {
            request_timeout_secs: DEFAULT_NETCAM_TIMEOUT_SECS,
            connect_timeout_ms: DEFAULT_NETCAM_CONNECT_TIMEOUT_MS,
            read_timeout_ms: DEFAULT_NETCAM_READ_TIMEOUT_MS,
            backoff_start_ms: DEFAULT_NETCAM_BACKOFF_START_MS,
            backoff_max_ms: DEFAULT_NETCAM_BACKOFF_MAX_MS,
            send_timeout_ms: DEFAULT_NETCAM_SEND_TIMEOUT_MS,
            stop_poll_ms: DEFAULT_NETCAM_STOP_POLL_MS,
        }
    }
}

impl NetcamTunables {
    pub(crate) fn sanitized(self) -> Self {
        let start = self.backoff_start_ms.max(100);
        let max = self.backoff_max_ms.max(start);
        Self {
            request_timeout_secs: self.request_timeout_secs.max(1),
            connect_timeout_ms: self.connect_timeout_ms.max(1),
            read_timeout_ms: self.read_timeout_ms.max(1),
            backoff_start_ms: start,
            backoff_max_ms: max,
            send_timeout_ms: self.send_timeout_ms.max(1),
            stop_poll_ms: self.stop_poll_ms.max(1),
        }
    }
}

/// Builder for request-local Styx tunables.
///
/// # Example
/// ```rust
/// use styx::prelude::*;
///
/// let config = StyxConfig::new()
///     .capture_queue_depth(8)
///     .capture_pool(4, 1 << 20, 8)
///     .netcam_timeouts(10);
/// ```
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct StyxConfig {
    capture: CaptureTunables,
    netcam: NetcamTunables,
    transform_pool: TransformPoolConfig,
}

impl StyxConfig {
    /// Start building a new configuration with defaults.
    pub fn new() -> Self {
        Self {
            capture: CaptureTunables::default(),
            netcam: NetcamTunables::default(),
            transform_pool: TransformPoolConfig::default(),
        }
    }

    /// Configuration biased toward fresh preview frames over completeness.
    pub fn low_latency_preview() -> Self {
        Self::new()
            .capture_queue_depth(1)
            .capture_pool(2, 1 << 18, 2)
    }

    /// Configuration biased toward preserving every frame during recording.
    pub fn reliable_recording() -> Self {
        Self::new()
            .capture_queue_depth(8)
            .capture_pool(4, 1 << 20, 8)
    }

    /// Configuration for bursty MJPEG-over-HTTP sources.
    pub fn netcam_preview() -> Self {
        Self::new()
            .netcam_timeouts(10)
            .netcam_backoff(500, 5_000)
            .capture_queue_depth(4)
    }

    /// Override capture queue depth.
    pub fn capture_queue_depth(mut self, depth: usize) -> Self {
        self.capture.queue_depth = depth;
        self
    }

    /// Override capture pool sizing.
    pub fn capture_pool(mut self, min: usize, bytes: usize, spare: usize) -> Self {
        self.capture.pool_min = min;
        self.capture.pool_bytes = bytes;
        self.capture.pool_spare = spare;
        self
    }

    /// Override the process-wide packed-transform pool sizing applied at capture startup.
    pub fn transform_pool(mut self, min: usize, bytes: usize, spare: usize) -> Self {
        self.transform_pool = TransformPoolConfig { min, bytes, spare };
        self
    }

    /// Override the generic capture frame enqueue timeout.
    pub fn capture_queue_send_timeout(mut self, timeout_ms: u64) -> Self {
        self.capture.queue_send_timeout_ms = timeout_ms;
        self
    }

    /// Override the decoded image cache limit used by the file backend.
    ///
    /// Set to `0` to disable decoded image caching.
    pub fn file_image_cache_bytes(mut self, bytes: usize) -> Self {
        self.capture.file_image_cache_bytes = bytes;
        self
    }

    /// Override V4L2 worker timing knobs.
    pub fn v4l2_worker_timing(
        mut self,
        mmap_poll_ms: u64,
        send_timeout_ms: u64,
        error_backoff_ms: u64,
    ) -> Self {
        self.capture.v4l2_mmap_poll_ms = mmap_poll_ms;
        self.capture.v4l2_send_timeout_ms = send_timeout_ms;
        self.capture.v4l2_error_backoff_ms = error_backoff_ms;
        self
    }

    /// Override libcamera worker timing knobs.
    pub fn libcamera_worker_timing(
        mut self,
        lookup_timeout_ms: u64,
        lookup_poll_ms: u64,
        requeue_stall_timeout_ms: u64,
        request_poll_ms: u64,
        idle_drain_timeout_ms: u64,
        idle_drain_poll_ms: u64,
    ) -> Self {
        self.capture.libcamera_lookup_timeout_ms = lookup_timeout_ms;
        self.capture.libcamera_lookup_poll_ms = lookup_poll_ms;
        self.capture.libcamera_requeue_stall_timeout_ms = requeue_stall_timeout_ms;
        self.capture.libcamera_request_poll_ms = request_poll_ms;
        self.capture.libcamera_idle_drain_timeout_ms = idle_drain_timeout_ms;
        self.capture.libcamera_idle_drain_poll_ms = idle_drain_poll_ms;
        self
    }

    /// Override how long libcamera control reads wait for a worker response.
    pub fn libcamera_control_response_timeout(mut self, timeout_ms: u64) -> Self {
        self.capture.libcamera_control_response_timeout_ms = timeout_ms;
        self
    }

    /// Override how long libcamera probe results are cached.
    ///
    /// Set to `0` to effectively bypass the cache.
    pub fn libcamera_probe_cache_ttl(mut self, ttl_ms: u64) -> Self {
        self.capture.libcamera_probe_cache_ttl_ms = ttl_ms;
        self
    }

    /// Override whether libcamera tries to stop its manager when a session goes idle.
    pub fn libcamera_stop_when_idle(mut self, enabled: bool) -> Self {
        self.capture.libcamera_stop_when_idle = enabled;
        self
    }

    /// Override whether libcamera request-pool backing memory is prefaulted at startup.
    pub fn libcamera_prefault_request_pools(mut self, enabled: bool) -> Self {
        self.capture.libcamera_prefault_request_pools = enabled;
        self
    }

    /// Override the processed stream role for libcamera display-like formats.
    pub fn libcamera_processed_stream_role(mut self, role: LibcameraProcessedStreamRole) -> Self {
        self.capture.libcamera_processed_stream_role = role;
        self
    }

    /// Override netcam request timeout.
    pub fn netcam_timeouts(mut self, request_secs: u64) -> Self {
        self.netcam.request_timeout_secs = request_secs;
        self
    }

    /// Override netcam HTTP request, connect, and read timeout knobs.
    pub fn netcam_http_timeouts(
        mut self,
        request_secs: u64,
        connect_ms: u64,
        read_ms: u64,
    ) -> Self {
        self.netcam.request_timeout_secs = request_secs;
        self.netcam.connect_timeout_ms = connect_ms;
        self.netcam.read_timeout_ms = read_ms;
        self
    }

    /// Override netcam backoff timings.
    pub fn netcam_backoff(mut self, start_ms: u64, max_ms: u64) -> Self {
        self.netcam.backoff_start_ms = start_ms;
        self.netcam.backoff_max_ms = max_ms;
        self
    }

    /// Override netcam frame enqueue timeout.
    pub fn netcam_send_timeout(mut self, timeout_ms: u64) -> Self {
        self.netcam.send_timeout_ms = timeout_ms;
        self
    }

    /// Override how often netcam workers wake while waiting for stop.
    pub fn netcam_stop_poll(mut self, poll_ms: u64) -> Self {
        self.netcam.stop_poll_ms = poll_ms;
        self
    }

    /// Return the sanitized capture tunables carried by this config.
    pub fn capture_tunables(&self) -> CaptureTunables {
        self.capture.sanitized()
    }

    /// Return the sanitized netcam tunables carried by this config.
    pub fn netcam_tunables(&self) -> NetcamTunables {
        self.netcam.sanitized()
    }

    /// Return the transform pool sizing carried by this config.
    pub fn transform_pool_config(&self) -> TransformPoolConfig {
        TransformPoolConfig {
            min: self.transform_pool.min,
            bytes: self.transform_pool.bytes.max(1),
            spare: self.transform_pool.spare,
        }
    }

    /// Apply process-wide runtime tunables that live outside an individual capture handle.
    pub fn apply_runtime_tunables(&self) {
        styx_core::transform::configure_transform_pool(self.transform_pool_config());
    }
}

impl Default for StyxConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libcamera_policy_tunables_round_trip_through_config() {
        let tunables = StyxConfig::new()
            .libcamera_stop_when_idle(true)
            .libcamera_prefault_request_pools(false)
            .libcamera_processed_stream_role(LibcameraProcessedStreamRole::VideoRecording)
            .libcamera_probe_cache_ttl(250)
            .capture_tunables();

        assert!(tunables.libcamera_stop_when_idle);
        assert!(!tunables.libcamera_prefault_request_pools);
        assert_eq!(
            tunables.libcamera_processed_stream_role,
            LibcameraProcessedStreamRole::VideoRecording
        );
        assert_eq!(tunables.libcamera_probe_cache_ttl_ms, 250);
    }

    #[test]
    fn netcam_http_timeout_tunables_round_trip_through_config() {
        let tunables = StyxConfig::new()
            .netcam_http_timeouts(7, 250, 750)
            .netcam_tunables();

        assert_eq!(tunables.request_timeout_secs, 7);
        assert_eq!(tunables.connect_timeout_ms, 250);
        assert_eq!(tunables.read_timeout_ms, 750);
    }

    #[test]
    fn transform_pool_tunables_round_trip_through_config() {
        let config = StyxConfig::new().transform_pool(3, 4096, 5);

        assert_eq!(
            config.transform_pool_config(),
            TransformPoolConfig {
                min: 3,
                bytes: 4096,
                spare: 5,
            }
        );
    }
}
