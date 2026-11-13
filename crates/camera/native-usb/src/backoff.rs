//! The retry schedule for a camera that is absent, failed or dropped.

use std::time::Duration;

/// The first retry after a failure.
pub const FIRST: Duration = Duration::from_millis(250);
/// The longest wait between two retries.
pub const CAP: Duration = Duration::from_secs(5);

/// Doubling waits from [`FIRST`] up to [`CAP`], counted since the last success. A camera that
/// keeps failing is retried for ever; nothing gives up.
#[derive(Debug, Clone)]
pub struct Backoff {
    next: Duration,
    failures: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next: FIRST,
            failures: 0,
        }
    }
}

impl Backoff {
    /// Records a failure and returns how long to wait before the next try.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(CAP);
        self.failures = self.failures.saturating_add(1);
        delay
    }

    /// Failures since the last [`reset`](Self::reset).
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Back to the first wait, after a success.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_from_the_first_wait_and_caps() {
        let mut backoff = Backoff::default();
        let delays: Vec<Duration> = (0..7).map(|_| backoff.next_delay()).collect();
        assert_eq!(
            delays,
            [
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_millis(1000),
                Duration::from_millis(2000),
                Duration::from_millis(4000),
                CAP,
                CAP,
            ]
        );
        assert_eq!(backoff.failures(), 7);
    }

    #[test]
    fn reset_starts_over() {
        let mut backoff = Backoff::default();
        backoff.next_delay();
        backoff.next_delay();
        backoff.reset();
        assert_eq!(backoff.failures(), 0);
        assert_eq!(backoff.next_delay(), FIRST);
    }
}
