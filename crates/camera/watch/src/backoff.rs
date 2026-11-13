//! Retry pacing for a source that failed: 5 s, then 10 s, and 10 s from then on. A source never
//! gives up; it waits and tries again.

use std::time::{Duration, Instant};

pub(crate) const FIRST: Duration = Duration::from_secs(5);
pub(crate) const CAP: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    next: Duration,
    due: Option<Instant>,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next: FIRST,
            due: None,
        }
    }
}

impl Backoff {
    /// Records a failure at `now` and returns how long until the next try.
    pub(crate) fn failed(&mut self, now: Instant) -> Duration {
        let wait = self.next;
        self.due = Some(now + wait);
        self.next = (self.next * 2).min(CAP);
        wait
    }

    /// Records a success: the next failure starts again at the first wait.
    pub(crate) fn succeeded(&mut self) {
        self.next = FIRST;
        self.due = None;
    }

    /// Whether a retry is owed at `now` (always true when nothing failed).
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.due.is_none_or(|due| now >= due)
    }

    /// Time until the retry is due, zero when it is due now.
    pub(crate) fn until(&self, now: Instant) -> Duration {
        self.due
            .map_or(Duration::ZERO, |due| due.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_from_five_seconds_to_a_cap_of_ten() {
        let mut b = Backoff::default();
        let now = Instant::now();
        assert_eq!(b.failed(now), Duration::from_secs(5));
        assert_eq!(b.failed(now), Duration::from_secs(10));
        assert_eq!(b.failed(now), Duration::from_secs(10));
        b.succeeded();
        assert_eq!(b.failed(now), Duration::from_secs(5));
    }

    #[test]
    fn due_only_after_the_wait() {
        let mut b = Backoff::default();
        let now = Instant::now();
        assert!(b.due(now));
        b.failed(now);
        assert!(!b.due(now));
        assert!(b.due(now + Duration::from_secs(5)));
    }
}
