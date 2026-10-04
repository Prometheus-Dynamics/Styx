//! Timestamps, and waits for a [`Duration`] over embedded-hal delays.
//!
//! There is no clock or delay trait here: delays are embedded-hal's `DelayNs` (blocking and
//! async), and the monotonic clock is the platform's (`CLOCK_MONOTONIC` on Linux,
//! `embassy-time`, a tick counter); receivers stamp their events with it.

use core::time::Duration;

use embedded_hal::delay::DelayNs;

/// A point on the platform's monotonic clock in nanoseconds: `CLOCK_MONOTONIC` on Linux (the
/// clock of V4L2 buffer and event timestamps), a tick counter scaled to nanoseconds on
/// microcontrollers. Receiver timestamps ([`SyncEvent`](crate::SyncEvent),
/// [`FrameDone`](crate::FrameDone)) use it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(pub u64);

impl Instant {
    /// From nanoseconds.
    pub const fn from_nanos(ns: u64) -> Self {
        Self(ns)
    }

    /// Nanoseconds.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// Time since `earlier`, zero if `earlier` is later.
    pub fn saturating_duration_since(self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    /// `self + d`, if it fits.
    pub fn checked_add(self, d: Duration) -> Option<Instant> {
        let ns = u64::try_from(d.as_nanos()).ok()?;
        self.0.checked_add(ns).map(Instant)
    }
}

/// Waits `duration` on a blocking embedded-hal delay (nanosecond steps up to ~4 s, longer
/// waits in milliseconds).
pub fn wait(delay: &mut impl DelayNs, duration: Duration) {
    match u32::try_from(duration.as_nanos()) {
        Ok(ns) => delay.delay_ns(ns),
        Err(_) => {
            let mut ms = duration.as_millis();
            while ms > 0 {
                let step = u32::try_from(ms).unwrap_or(u32::MAX);
                delay.delay_ms(step);
                ms -= u128::from(step);
            }
        }
    }
}

/// [`wait`] on an async embedded-hal delay.
pub async fn wait_async(delay: &mut impl embedded_hal_async::delay::DelayNs, duration: Duration) {
    match u32::try_from(duration.as_nanos()) {
        Ok(ns) => delay.delay_ns(ns).await,
        Err(_) => {
            let mut ms = duration.as_millis();
            while ms > 0 {
                let step = u32::try_from(ms).unwrap_or(u32::MAX);
                delay.delay_ms(step).await;
                ms -= u128::from(step);
            }
        }
    }
}
