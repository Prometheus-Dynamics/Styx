//! The clock kernel timestamps use.

use std::time::Duration;

/// `CLOCK_MONOTONIC` now: the clock of V4L2 buffer and event timestamps, so the time since a
/// frame started is `monotonic_now() - timestamp`.
pub fn monotonic_now() -> Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is valid writable storage for `clock_gettime`, which only writes it.
    let r = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    if r != 0 {
        return Duration::ZERO;
    }
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_moves_forward() {
        let a = monotonic_now();
        std::thread::sleep(Duration::from_millis(2));
        let b = monotonic_now();
        assert!(b > a && b - a >= Duration::from_millis(2));
    }
}
