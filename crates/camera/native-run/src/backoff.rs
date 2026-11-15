//! The failure policy: how long to wait before a retry, and whether to give up.

use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

/// The shortest cap the policy allows.
pub const CAP_MIN: Duration = Duration::from_secs(5);
/// The longest cap the policy allows.
pub const CAP_MAX: Duration = Duration::from_secs(10);

/// Why a backoff was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffError {
    /// The cap is outside 5 to 10 s.
    CapOutOfRange(Duration),
    /// The first wait is zero, or longer than the cap.
    FirstOutOfRange(Duration),
}

impl fmt::Display for BackoffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapOutOfRange(cap) => write!(f, "backoff cap {cap:?} is outside 5 to 10 s"),
            Self::FirstOutOfRange(first) => {
                write!(f, "first backoff {first:?} is zero or past the cap")
            }
        }
    }
}

impl std::error::Error for BackoffError {}

/// Waits between retries: the first wait, doubling after each failure in a row, never past the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    first: Duration,
    cap: Duration,
}

impl Default for Backoff {
    /// 250 ms, doubling, capped at 5 s.
    fn default() -> Self {
        Self {
            first: Duration::from_millis(250),
            cap: CAP_MIN,
        }
    }
}

impl Backoff {
    /// A backoff starting at `first` and capped at `cap`. `cap` must be 5 to 10 s.
    pub fn new(first: Duration, cap: Duration) -> Result<Self, BackoffError> {
        if !(CAP_MIN..=CAP_MAX).contains(&cap) {
            return Err(BackoffError::CapOutOfRange(cap));
        }
        if first.is_zero() || first > cap {
            return Err(BackoffError::FirstOutOfRange(first));
        }
        Ok(Self { first, cap })
    }

    /// The first wait.
    pub fn first(&self) -> Duration {
        self.first
    }

    /// The longest wait.
    pub fn cap(&self) -> Duration {
        self.cap
    }

    /// The wait after `failures` failures in a row (1 for the first failure).
    pub fn delay(&self, failures: u32) -> Duration {
        // Past 2^31 times the first wait every cap the policy allows is reached, so the shift stops
        // there and the multiplication cannot overflow into a wrong answer.
        let doublings = failures.saturating_sub(1).min(31);
        self.first.saturating_mul(1u32 << doublings).min(self.cap)
    }
}

/// When a session stops retrying on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoOff {
    /// Never. A camera that drops comes back. The default.
    #[default]
    Never,
    /// After this many failures in a row, the session turns itself off until it is started again.
    AfterFailures(NonZeroU32),
}

/// What a session does when it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub backoff: Backoff,
    pub auto_off: AutoOff,
    /// How long one wait for a frame lasts.
    pub frame_timeout: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            backoff: Backoff::default(),
            auto_off: AutoOff::Never,
            frame_timeout: Duration::from_millis(250),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_then_hold_at_the_cap() {
        let backoff = Backoff::default();
        let delays: Vec<Duration> = (1..=8).map(|n| backoff.delay(n)).collect();
        let want: Vec<Duration> = [250, 500, 1000, 2000, 4000, 5000, 5000, 5000]
            .map(Duration::from_millis)
            .to_vec();
        assert_eq!(delays, want);
    }

    #[test]
    fn a_huge_failure_count_is_still_the_cap() {
        let backoff = Backoff::new(Duration::from_millis(1), Duration::from_secs(10)).unwrap();
        assert_eq!(backoff.delay(u32::MAX), Duration::from_secs(10));
        assert_eq!(backoff.delay(0), Duration::from_millis(1));
    }

    #[test]
    fn the_cap_must_be_five_to_ten_seconds() {
        let first = Duration::from_millis(250);
        assert!(Backoff::new(first, Duration::from_secs(4)).is_err());
        assert!(Backoff::new(first, Duration::from_secs(11)).is_err());
        assert!(Backoff::new(first, Duration::from_secs(5)).is_ok());
        assert!(Backoff::new(first, Duration::from_secs(10)).is_ok());
        assert!(Backoff::new(Duration::ZERO, Duration::from_secs(5)).is_err());
        assert!(Backoff::new(Duration::from_secs(6), Duration::from_secs(5)).is_err());
    }
}
