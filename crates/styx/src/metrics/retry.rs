use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureRetryStats {
    pub start_retry_count: u64,
    /// Attempts to reconnect a live source after it disconnected or stalled.
    pub reconnect_attempts: u64,
    /// Times frames resumed after reconnect attempts.
    pub reconnects: u64,
    /// Time without frames before the last reconnect, in milliseconds (last frame before the
    /// disconnect to the first frame after it).
    pub last_reconnect_downtime_ms: Option<u64>,
    pub last_retry_reason: Option<String>,
    pub last_retry_error: Option<String>,
    pub last_successful_frame_unix_ms: Option<u128>,
    /// Times streaming stopped because nobody pulled frames (`StyxConfig::stop_when_idle`).
    pub idle_stops: u64,
    /// Times streaming started again on a pull after an idle stop.
    pub idle_resumes: u64,
}

#[derive(Clone, Default)]
pub struct CaptureRetryMetrics {
    inner: Arc<Mutex<CaptureRetryStats>>,
    /// Set by the first reconnect attempt, cleared by the next successful frame.
    disconnected_since: Arc<Mutex<Option<std::time::Instant>>>,
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

    /// Start the downtime of the next reconnect at `since` (the last frame) unless one is
    /// already running.
    pub(crate) fn record_disconnected_since(&self, since: std::time::Instant) {
        self.disconnected_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert(since);
    }

    pub fn record_reconnect_attempt(&self, reason: impl Into<String>, error: impl Into<String>) {
        self.disconnected_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert_with(std::time::Instant::now);
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.reconnect_attempts = stats.reconnect_attempts.saturating_add(1);
        stats.last_retry_reason = Some(reason.into());
        stats.last_retry_error = Some(error.into());
    }

    pub(crate) fn record_idle_stop(&self) {
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.idle_stops = stats.idle_stops.saturating_add(1);
    }

    pub(crate) fn record_idle_resume(&self) {
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.idle_resumes = stats.idle_resumes.saturating_add(1);
    }

    pub fn record_successful_frame(&self) {
        let downtime = self
            .disconnected_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .map(|since| since.elapsed());
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.last_successful_frame_unix_ms = Some(now_unix_ms());
        if let Some(downtime) = downtime {
            stats.reconnects = stats.reconnects.saturating_add(1);
            stats.last_reconnect_downtime_ms = Some(downtime.as_millis() as u64);
        }
    }

    pub fn merge_snapshot(&self, snapshot: CaptureRetryStats) {
        let mut stats = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.start_retry_count = stats
            .start_retry_count
            .saturating_add(snapshot.start_retry_count);
        stats.reconnect_attempts = stats
            .reconnect_attempts
            .saturating_add(snapshot.reconnect_attempts);
        stats.reconnects = stats.reconnects.saturating_add(snapshot.reconnects);
        stats.idle_stops = stats.idle_stops.saturating_add(snapshot.idle_stops);
        stats.idle_resumes = stats.idle_resumes.saturating_add(snapshot.idle_resumes);
        if snapshot.last_reconnect_downtime_ms.is_some() {
            stats.last_reconnect_downtime_ms = snapshot.last_reconnect_downtime_ms;
        }
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

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
