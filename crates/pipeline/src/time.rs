//! Timestamps for the loop's timings: `std::time::Instant` with `std`, nothing without (the
//! timings are then zero).

use core::time::Duration;

/// A point in time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stamp(#[cfg(feature = "std")] std::time::Instant);

/// Now.
#[inline]
pub(crate) fn now() -> Stamp {
    #[cfg(feature = "std")]
    return Stamp(std::time::Instant::now());
    #[cfg(not(feature = "std"))]
    Stamp()
}

impl Stamp {
    /// Time from `earlier` to `self`.
    #[inline]
    pub(crate) fn since(self, earlier: Stamp) -> Duration {
        #[cfg(feature = "std")]
        return self.0.saturating_duration_since(earlier.0);
        #[cfg(not(feature = "std"))]
        {
            let _ = earlier;
            Duration::ZERO
        }
    }
}
