use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureRetryStats {
    pub start_retry_count: u64,
    pub netcam_retry_count: u64,
    pub last_retry_reason: Option<String>,
    pub last_retry_error: Option<String>,
    pub last_successful_frame_unix_ms: Option<u128>,
}

#[derive(Clone, Default)]
pub struct CaptureRetryMetrics {
    inner: Arc<Mutex<CaptureRetryStats>>,
}

impl CaptureRetryMetrics {
    pub fn record_start_retry(&self, reason: impl Into<String>, error: impl Into<String>) {
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.start_retry_count = stats.start_retry_count.saturating_add(1);
        stats.last_retry_reason = Some(reason.into());
        stats.last_retry_error = Some(error.into());
    }

    #[cfg(feature = "netcam")]
    pub fn record_netcam_retry(&self, reason: impl Into<String>, error: impl Into<String>) {
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.netcam_retry_count = stats.netcam_retry_count.saturating_add(1);
        stats.last_retry_reason = Some(reason.into());
        stats.last_retry_error = Some(error.into());
    }

    #[cfg(feature = "netcam")]
    pub fn record_successful_frame(&self) {
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.last_successful_frame_unix_ms = Some(now_unix_ms());
    }

    pub fn merge_snapshot(&self, snapshot: CaptureRetryStats) {
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.start_retry_count = stats
            .start_retry_count
            .saturating_add(snapshot.start_retry_count);
        stats.netcam_retry_count = stats
            .netcam_retry_count
            .saturating_add(snapshot.netcam_retry_count);
        if snapshot.last_retry_reason.is_some() {
            stats.last_retry_reason = snapshot.last_retry_reason;
        }
        if snapshot.last_retry_error.is_some() {
            stats.last_retry_error = snapshot.last_retry_error;
        }
        if snapshot.last_successful_frame_unix_ms.is_some() {
            stats.last_successful_frame_unix_ms = snapshot.last_successful_frame_unix_ms;
        }
    }

    pub fn snapshot(&self) -> CaptureRetryStats {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(feature = "netcam")]
fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
