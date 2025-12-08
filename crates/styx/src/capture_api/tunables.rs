use std::sync::{Mutex, OnceLock};

/// Default capture queue depth (frames).
pub const DEFAULT_QUEUE_DEPTH: usize = 4;
/// Default buffer pool minimum count.
pub const DEFAULT_POOL_MIN: usize = 4;
/// Default buffer pool bytes per buffer.
pub const DEFAULT_POOL_BYTES: usize = 1 << 20;
/// Default extra spare buffers beyond the minimum.
pub const DEFAULT_POOL_SPARE: usize = 8;
/// Default netcam request timeout (seconds).
pub const DEFAULT_NETCAM_TIMEOUT_SECS: u64 = 5;
/// Default netcam backoff start delay (milliseconds).
pub const DEFAULT_NETCAM_BACKOFF_START_MS: u64 = 1_000;
/// Default netcam backoff maximum delay (milliseconds).
pub const DEFAULT_NETCAM_BACKOFF_MAX_MS: u64 = 10_000;

/// Tunables for capture queues and buffer pools.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// set_capture_tunables(CaptureTunables {
///     queue_depth: 8,
///     pool_min: 6,
///     pool_bytes: 2 << 20,
///     pool_spare: 8,
/// });
/// ```
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct CaptureTunables {
    pub queue_depth: usize,
    pub pool_min: usize,
    pub pool_bytes: usize,
    pub pool_spare: usize,
}

impl Default for CaptureTunables {
    fn default() -> Self {
        Self {
            queue_depth: DEFAULT_QUEUE_DEPTH,
            pool_min: DEFAULT_POOL_MIN,
            pool_bytes: DEFAULT_POOL_BYTES,
            pool_spare: DEFAULT_POOL_SPARE,
        }
    }
}

impl CaptureTunables {
    fn sanitized(self) -> Self {
        Self {
            queue_depth: self.queue_depth.max(1),
            pool_min: self.pool_min.max(1),
            pool_bytes: self.pool_bytes.max(1),
            pool_spare: self.pool_spare,
        }
    }
}

static CAPTURE_TUNABLES: OnceLock<Mutex<CaptureTunables>> = OnceLock::new();
static NETCAM_TUNABLES: OnceLock<Mutex<NetcamTunables>> = OnceLock::new();

/// Override capture tunables process-wide.
pub fn set_capture_tunables(tunables: CaptureTunables) {
    let lock = CAPTURE_TUNABLES.get_or_init(|| Mutex::new(CaptureTunables::default()));
    *lock.lock().unwrap() = tunables.sanitized();
}

pub(crate) fn capture_queue_depth() -> usize {
    CAPTURE_TUNABLES
        .get()
        .and_then(|t| t.lock().ok().map(|v| v.queue_depth))
        .unwrap_or(DEFAULT_QUEUE_DEPTH)
}

pub(crate) fn capture_pool_limits(
    default_min: usize,
    default_bytes: usize,
    default_spare: usize,
) -> (usize, usize, usize) {
    if let Some(lock) = CAPTURE_TUNABLES.get()
        && let Ok(t) = lock.lock()
    {
        return (t.pool_min, t.pool_bytes, t.pool_spare);
    }
    (default_min, default_bytes, default_spare)
}

/// Tunables for netcam polling/backoff behavior.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct NetcamTunables {
    pub request_timeout_secs: u64,
    pub backoff_start_ms: u64,
    pub backoff_max_ms: u64,
}

impl Default for NetcamTunables {
    fn default() -> Self {
        Self {
            request_timeout_secs: DEFAULT_NETCAM_TIMEOUT_SECS,
            backoff_start_ms: DEFAULT_NETCAM_BACKOFF_START_MS,
            backoff_max_ms: DEFAULT_NETCAM_BACKOFF_MAX_MS,
        }
    }
}

impl NetcamTunables {
    fn sanitized(self) -> Self {
        let start = self.backoff_start_ms.max(100);
        let max = self.backoff_max_ms.max(start);
        Self {
            request_timeout_secs: self.request_timeout_secs.max(1),
            backoff_start_ms: start,
            backoff_max_ms: max,
        }
    }
}

/// Override netcam tunables process-wide.
pub fn set_netcam_tunables(tunables: NetcamTunables) {
    let lock = NETCAM_TUNABLES.get_or_init(|| Mutex::new(NetcamTunables::default()));
    *lock.lock().unwrap() = tunables.sanitized();
}

#[allow(dead_code)]
pub(crate) fn netcam_tunables() -> NetcamTunables {
    NETCAM_TUNABLES
        .get()
        .and_then(|t| t.lock().ok().map(|v| v.sanitized()))
        .unwrap_or_default()
}

/// Builder for process-wide Styx tunables.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// StyxConfig::new()
///     .capture_queue_depth(8)
///     .capture_pool(4, 1 << 20, 8)
///     .netcam_timeouts(10)
///     .apply();
/// ```
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct StyxConfig {
    capture: CaptureTunables,
    netcam: NetcamTunables,
}

impl StyxConfig {
    /// Start building a new configuration with defaults.
    pub fn new() -> Self {
        Self {
            capture: CaptureTunables::default(),
            netcam: NetcamTunables::default(),
        }
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

    /// Override netcam request timeout.
    pub fn netcam_timeouts(mut self, request_secs: u64) -> Self {
        self.netcam.request_timeout_secs = request_secs;
        self
    }

    /// Override netcam backoff timings.
    pub fn netcam_backoff(mut self, start_ms: u64, max_ms: u64) -> Self {
        self.netcam.backoff_start_ms = start_ms;
        self.netcam.backoff_max_ms = max_ms;
        self
    }

    /// Apply the configuration to global tunables.
    pub fn apply(self) {
        set_capture_tunables(self.capture);
        set_netcam_tunables(self.netcam);
    }
}

impl Default for StyxConfig {
    fn default() -> Self {
        Self::new()
    }
}
