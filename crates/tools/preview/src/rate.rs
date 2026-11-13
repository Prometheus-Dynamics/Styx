//! The rate cap: which source frames a preview encodes, decided from the frames' own
//! timestamps (`CLOCK_MONOTONIC` nanoseconds), so the cap is the same whatever the clock source.

/// Admits frames at most `max_fps` per second, spread evenly.
///
/// The first frame is admitted. After that a frame is admitted when its timestamp reaches the
/// next due time, less a tolerance of one eighth of an interval (so a 60 fps source capped at 15
/// gives every fourth frame even when the timestamps jitter). After a gap the due time restarts
/// from the frame that ends it, not from the past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateCap {
    interval_ns: u64,
    tolerance_ns: u64,
    /// The next due time; `None` until the first frame.
    next_due_ns: Option<u64>,
}

impl RateCap {
    /// A cap at `max_fps`. The caller checks that `max_fps` is finite and positive.
    pub fn new(max_fps: f64) -> Self {
        let interval_ns = (1_000_000_000.0 / max_fps).round() as u64;
        let interval_ns = interval_ns.max(1);
        Self {
            interval_ns,
            tolerance_ns: interval_ns / 8,
            next_due_ns: None,
        }
    }

    /// The interval between admitted frames, in nanoseconds.
    pub const fn interval_ns(&self) -> u64 {
        self.interval_ns
    }

    /// Whether the frame stamped `timestamp_ns` is encoded. Dropped frames cost nothing else.
    pub fn admit(&mut self, timestamp_ns: u64) -> bool {
        let Some(due) = self.next_due_ns else {
            self.next_due_ns = Some(timestamp_ns.saturating_add(self.interval_ns));
            return true;
        };
        if timestamp_ns.saturating_add(self.tolerance_ns) < due {
            return false;
        }
        let on_schedule = due.saturating_add(self.interval_ns);
        let after_gap = timestamp_ns
            .saturating_add(self.interval_ns)
            .saturating_sub(self.tolerance_ns);
        self.next_due_ns = Some(on_schedule.max(after_gap));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::RateCap;

    #[test]
    fn sixty_to_fifteen_takes_every_fourth_frame() {
        let mut cap = RateCap::new(15.0);
        // A 60 fps source with nanosecond timestamps from a monotonic clock.
        let admitted: Vec<usize> = (0..240_usize)
            .filter(|&i| cap.admit(1_000_000_000 + i as u64 * 16_666_667))
            .collect();
        assert_eq!(admitted.len(), 60);
        assert!(
            admitted.windows(2).all(|w| w[1] - w[0] == 4),
            "{admitted:?}"
        );
    }

    #[test]
    fn jittered_timestamps_still_keep_the_rate() {
        let mut cap = RateCap::new(15.0);
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut t = 0_u64;
        let mut admitted = 0;
        for _ in 0..600 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            t += 16_666_667 + state % 2_000_000 - 1_000_000;
            admitted += usize::from(cap.admit(t));
        }
        assert!((148..=152).contains(&admitted), "{admitted}");
    }

    #[test]
    fn a_gap_restarts_the_schedule_from_the_frame_that_ends_it() {
        let mut cap = RateCap::new(10.0);
        assert!(cap.admit(0));
        // Ten seconds of nothing, then a frame: admitted, and the next one is one interval on.
        assert!(cap.admit(10_000_000_000));
        assert!(!cap.admit(10_000_000_000 + 50_000_000));
        assert!(cap.admit(10_000_000_000 + 100_000_000));
    }
}
