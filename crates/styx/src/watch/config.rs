use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchRuntimeConfig {
    pub max_retained_events: usize,
    pub max_retained_event_bytes: Option<usize>,
    pub watch_settle_time: Duration,
}

impl Default for WatchRuntimeConfig {
    fn default() -> Self {
        Self {
            max_retained_events: 256,
            max_retained_event_bytes: Some(512 * 1024),
            watch_settle_time: Duration::from_millis(500),
        }
    }
}
