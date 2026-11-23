//! The clocks a recording's timestamps are on.
//!
//! Every timestamp in a recording, frames and samples alike, is nanoseconds of `CLOCK_MONOTONIC`
//! (the clock Styx stamps frames with on every backend). A producer whose samples are stamped
//! on `CLOCK_BOOTTIME` (which also counts time suspended) converts them with
//! [`monotonic_from_boottime`] or a [`ClockPair`] before recording them. The two clocks differ
//! by the time the machine has spent suspended; on a machine that never suspends the difference
//! stays fixed, and a pair read once converts every later sample.

/// `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` read together, in nanoseconds. A channel declaration
/// records the pair read when it was made, so the offset between the clocks at that moment
/// stays in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockPair {
    /// `CLOCK_MONOTONIC`, nanoseconds.
    pub monotonic_ns: u64,
    /// `CLOCK_BOOTTIME`, nanoseconds. Zero where the system has no such clock (it is Linux's).
    pub boottime_ns: u64,
}

impl ClockPair {
    /// Both clocks now. The pair is read three times as monotonic, boottime, monotonic, and the
    /// tightest read is kept, with the boottime set against the middle of its two monotonic
    /// reads, so the two values are within one clock read of the same instant.
    ///
    /// On a system without `CLOCK_BOOTTIME` (not Linux), `boottime_ns` is zero; on one without
    /// either clock both are.
    #[must_use]
    pub fn now() -> Self {
        let mut best = Self::default();
        let mut best_gap = u64::MAX;
        for _ in 0..3 {
            let before = monotonic_now();
            let boottime_ns = boottime_now();
            let after = monotonic_now();
            let gap = after.saturating_sub(before);
            if gap < best_gap {
                best_gap = gap;
                best = Self {
                    monotonic_ns: before + gap / 2,
                    boottime_ns,
                };
            }
        }
        best
    }

    /// `CLOCK_BOOTTIME` minus `CLOCK_MONOTONIC` at this pair: the time spent suspended before
    /// it was read. Zero when the pair has no boottime, and when the read's own spread puts
    /// boottime a few nanoseconds behind (a machine that has never suspended).
    #[must_use]
    pub const fn suspended_ns(&self) -> u64 {
        self.boottime_ns.saturating_sub(self.monotonic_ns)
    }

    /// A `CLOCK_BOOTTIME` time on `CLOCK_MONOTONIC`, with the offset this pair was read at. Right
    /// for any time since the pair was read, as long as the machine has not suspended between
    /// the two. A time before boot's zero saturates at zero.
    #[must_use]
    pub const fn monotonic_from_boottime(&self, boottime_ns: u64) -> u64 {
        boottime_ns.saturating_sub(self.suspended_ns())
    }
}

/// A `CLOCK_BOOTTIME` time on `CLOCK_MONOTONIC`, with the offset between the clocks read now
/// ([`ClockPair::now`]). For a stream of samples, read a [`ClockPair`] once and convert with
/// [`ClockPair::monotonic_from_boottime`]: the offset only moves when the machine suspends.
#[must_use]
pub fn monotonic_from_boottime(boottime_ns: u64) -> u64 {
    ClockPair::now().monotonic_from_boottime(boottime_ns)
}

#[cfg(unix)]
fn nanos(time: rustix::time::Timespec) -> u64 {
    // Both clocks are non-negative; a negative value would be a broken kernel.
    let seconds = u64::try_from(time.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(time.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

#[cfg(unix)]
fn monotonic_now() -> u64 {
    nanos(rustix::time::clock_gettime(
        rustix::time::ClockId::Monotonic,
    ))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn boottime_now() -> u64 {
    nanos(rustix::time::clock_gettime(rustix::time::ClockId::Boottime))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn boottime_now() -> u64 {
    0
}

#[cfg(not(unix))]
fn monotonic_now() -> u64 {
    0
}

#[cfg(not(unix))]
fn boottime_now() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::{ClockPair, monotonic_from_boottime};

    #[test]
    fn a_pair_converts_by_its_offset() {
        let pair = ClockPair {
            monotonic_ns: 1_000,
            boottime_ns: 1_250,
        };
        assert_eq!(pair.suspended_ns(), 250);
        assert_eq!(pair.monotonic_from_boottime(5_250), 5_000);
        assert_eq!(pair.monotonic_from_boottime(100), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_clocks_agree_now() {
        let pair = ClockPair::now();
        assert!(pair.monotonic_ns > 0);
        // Equal on a machine that has never suspended, up to the read's own spread (the
        // monotonic value is the middle of two reads around the boottime one).
        assert!(pair.boottime_ns + 1_000_000 >= pair.monotonic_ns);
        // A boottime read now lands within a few ms of a monotonic read now (the test
        // machine does not suspend while it runs).
        let before = super::monotonic_now();
        let converted = monotonic_from_boottime(super::boottime_now());
        let after = super::monotonic_now();
        assert!(converted + 5_000_000 >= before && converted <= after + 5_000_000);
    }
}
