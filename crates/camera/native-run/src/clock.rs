//! Time for a session: what time it is, and waiting out a backoff.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Time as a session sees it. Tests give a session a clock that does not wait.
pub trait Clock {
    /// Monotonic time since the clock started.
    fn now(&self) -> Duration;

    /// Waits for `for_`, or returns early once `cancel` is set.
    fn wait(&self, for_: Duration, cancel: &AtomicBool);
}

impl<T: Clock + ?Sized> Clock for &T {
    fn now(&self) -> Duration {
        (**self).now()
    }

    fn wait(&self, for_: Duration, cancel: &AtomicBool) {
        (**self).wait(for_, cancel);
    }
}

/// How often a real wait looks at its cancel flag.
const POLL: Duration = Duration::from_millis(20);

/// The machine's monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn wait(&self, for_: Duration, cancel: &AtomicBool) {
        let until = Instant::now() + for_;
        loop {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            std::thread::sleep(left.min(POLL));
        }
    }
}
