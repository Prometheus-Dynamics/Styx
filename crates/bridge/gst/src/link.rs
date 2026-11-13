//! The producer link's state, shared by the source's methods: the backoff (250 ms doubling to 5 s,
//! reset on a connect) and what the source reports. Time is an argument everywhere, so the backoff
//! is tested with a fake clock. The PipeWire node keeps the same state machine in its own crate.

use std::time::{Duration, Instant};

/// The first delay after a failure.
pub const BACKOFF_START: Duration = Duration::from_millis(250);
/// The longest delay between attempts.
pub const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// Exponential backoff. The delay doubles on each failure up to [`BACKOFF_CAP`]; a success
/// resets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    next: Duration,
    retry_at: Option<Instant>,
}

impl Backoff {
    /// No failure yet: an attempt is due at once.
    pub const fn new() -> Self {
        Self {
            next: BACKOFF_START,
            retry_at: None,
        }
    }

    /// Whether an attempt is due at `now`.
    pub fn due(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|at| now >= at)
    }

    /// Records a failure at `now`. Returns the delay until the next attempt.
    pub fn failed(&mut self, now: Instant) -> Duration {
        let wait = self.next;
        self.retry_at = Some(now + wait);
        self.next = (self.next * 2).min(BACKOFF_CAP);
        wait
    }

    /// Records a success: the next failure starts again at [`BACKOFF_START`].
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// What the link is doing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Trying to connect to the producer.
    #[default]
    Connecting,
    /// Connected to a producer whose frames flow.
    Streaming,
    /// The producer is gone or refused; the next attempt waits out the backoff.
    Waiting,
    /// Stopped by the caller.
    Stopped,
}

/// A snapshot of the link, for the caller.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LinkStatus {
    pub state: LinkState,
    /// Connections made (each one a producer that answered its handshake).
    pub connects: u64,
    /// Failed connects and lost connections.
    pub failures: u64,
    /// While waiting: the delay until the next attempt.
    pub next_delay: Option<Duration>,
    /// The last failure's message.
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_250_ms_to_the_5_s_cap_and_resets() {
        let t0 = Instant::now();
        let mut backoff = Backoff::new();
        let ms: Vec<u128> = (0..7).map(|_| backoff.failed(t0).as_millis()).collect();
        assert_eq!(ms, [250, 500, 1000, 2000, 4000, 5000, 5000]);
        backoff.reset();
        assert_eq!(backoff.failed(t0).as_millis(), 250);
    }
}
