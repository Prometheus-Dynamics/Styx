//! One camera's frame-path counters. Every update is an atomic operation: no lock, no allocation.
//!
//! One thread (the camera's) records frames; the publisher reads and resets the window at its own
//! low rate. A read that races a frame may count that frame in the next window, never twice.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// A frame's identity and timestamp, written by the frame path and read by the publisher. A
/// single-writer seqlock: the writer never waits, and a reader retries a read that overlapped a
/// write, so the pair is always one frame's.
#[derive(Debug, Default)]
struct Pair {
    seq: AtomicU64,
    id: AtomicU64,
    ns: AtomicU64,
}

impl Pair {
    /// Stores one frame's pair. Only the camera's thread calls this.
    fn set(&self, id: u64, ns: u64) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.id.store(id, Ordering::Relaxed);
        self.ns.store(ns, Ordering::Relaxed);
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// The last stored pair, never torn.
    fn get(&self) -> (u64, u64) {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let id = self.id.load(Ordering::Relaxed);
            let ns = self.ns.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return (id, ns);
            }
        }
    }
}

use crate::snapshot::{AGE_BUCKETS, DROP_REASONS, DropReason};

/// What the window held when it was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Frames delivered in the window.
    pub frames: u64,
    pub min_age_ns: u64,
    pub max_age_ns: u64,
    pub hist: [u32; AGE_BUCKETS],
    /// The first and last frame of the window: identity and capture timestamp (`CLOCK_MONOTONIC`).
    pub first_id: u64,
    pub first_ns: u64,
    pub last_id: u64,
    pub last_ns: u64,
}

/// The counters of one camera. `Default` is all zero.
#[derive(Debug)]
pub struct CameraCounters {
    frames: AtomicU64,
    last: Pair,
    /// Drops recorded (for a reason other than unattributed) since the last delivered frame. They
    /// explain identity gaps: a gap the caller has already counted is not counted again.
    explained: AtomicU64,
    drops: [AtomicU64; DROP_REASONS],
    age_min: AtomicU64,
    age_max: AtomicU64,
    age_hist: [AtomicU32; AGE_BUCKETS],
    window_frames: AtomicU64,
    window_first: Pair,
    window_last: Pair,
}

impl Default for CameraCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraCounters {
    /// Counters with nothing recorded.
    pub fn new() -> Self {
        Self {
            frames: AtomicU64::new(0),
            last: Pair::default(),
            explained: AtomicU64::new(0),
            drops: std::array::from_fn(|_| AtomicU64::new(0)),
            age_min: AtomicU64::new(u64::MAX),
            age_max: AtomicU64::new(0),
            age_hist: std::array::from_fn(|_| AtomicU32::new(0)),
            window_frames: AtomicU64::new(0),
            window_first: Pair::default(),
            window_last: Pair::default(),
        }
    }

    /// Records one delivered frame: its identity, its capture timestamp, and `now_ns`, the
    /// `CLOCK_MONOTONIC` time it reached the publish point.
    ///
    /// A jump in identities since the last frame is a run of frames the stream did not deliver. The
    /// drops recorded since then ([`CameraCounters::record_drop`]) explain that many of them; the
    /// rest count as [`DropReason::Unattributed`].
    pub fn record_frame(&self, id: u64, timestamp_ns: u64, now_ns: u64) {
        let explained = self.explained.swap(0, Ordering::Relaxed);
        let before = self.frames.fetch_add(1, Ordering::Relaxed);
        if before > 0 {
            let gap = id.saturating_sub(self.last.get().0 + 1);
            let unexplained = gap.saturating_sub(explained);
            if unexplained > 0 {
                self.drops[DropReason::Unattributed as usize]
                    .fetch_add(unexplained, Ordering::Relaxed);
            }
        }
        self.last.set(id, timestamp_ns);

        if self.window_frames.fetch_add(1, Ordering::Relaxed) == 0 {
            self.window_first.set(id, timestamp_ns);
        }
        self.window_last.set(id, timestamp_ns);

        let age = now_ns.saturating_sub(timestamp_ns);
        self.age_min.fetch_min(age, Ordering::Relaxed);
        self.age_max.fetch_max(age, Ordering::Relaxed);
        self.age_hist[bucket(age)].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one frame the camera did not deliver, for `reason`. A frame the stream never
    /// delivered is seen as a gap in identities; this reason explains that gap.
    pub fn record_drop(&self, reason: DropReason) {
        self.drops[reason as usize].fetch_add(1, Ordering::Relaxed);
        if reason != DropReason::Unattributed {
            self.explained.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Frames delivered since the counters were made.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Drops recorded for `reason`.
    pub fn drops(&self, reason: DropReason) -> u64 {
        self.drops[reason as usize].load(Ordering::Relaxed)
    }

    /// The last delivered frame's identity and timestamp (zero before the first frame).
    pub fn last(&self) -> (u64, u64) {
        self.last.get()
    }

    /// Takes the window and starts the next one. Called by the publisher, not the frame path.
    pub fn take_window(&self) -> Window {
        let frames = self.window_frames.swap(0, Ordering::Relaxed);
        let min = self.age_min.swap(u64::MAX, Ordering::Relaxed);
        let max = self.age_max.swap(0, Ordering::Relaxed);
        let hist = std::array::from_fn(|i| self.age_hist[i].swap(0, Ordering::Relaxed));
        let (first_id, first_ns) = self.window_first.get();
        let (last_id, last_ns) = self.window_last.get();
        Window {
            frames,
            min_age_ns: if frames == 0 { 0 } else { min },
            max_age_ns: max,
            hist,
            first_id,
            first_ns,
            last_id,
            last_ns,
        }
    }
}

/// The age histogram's bucket: `i` for ages in `[2^i, 2^(i+1))` ns, saturating at the last.
fn bucket(age_ns: u64) -> usize {
    let log2 = 63 - (age_ns | 1).leading_zeros() as usize;
    log2.min(AGE_BUCKETS - 1)
}

/// The median age of a window, as the upper bound of its median's bucket, at most `max_ns`.
pub fn median_of(hist: &[u32; AGE_BUCKETS], count: u64, max_ns: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    let target = count.div_ceil(2);
    let mut seen = 0_u64;
    for (i, n) in hist.iter().enumerate() {
        seen += u64::from(*n);
        if seen >= target {
            let upper = if i + 1 >= 64 {
                u64::MAX
            } else {
                1_u64 << (i + 1)
            };
            return upper.min(max_ns);
        }
    }
    max_ns
}

/// The achieved frame rate of a window, in thousandths of a frame per second: the frame slots the
/// window spans (its identities, so a frame the stream dropped still counts as a slot of the
/// sensor's cadence) over the time between its first and last frame. Zero with fewer than two
/// frames.
pub fn achieved_fps_milli(first_id: u64, first_ns: u64, last_id: u64, last_ns: u64) -> u32 {
    if last_id <= first_id || last_ns <= first_ns {
        return 0;
    }
    let milli = u128::from(last_id - first_id) * 1_000_000_000_000 / u128::from(last_ns - first_ns);
    u32::try_from(milli).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_gaps_and_drops_are_counted_once() {
        let c = CameraCounters::new();
        c.record_frame(10, 1_000, 1_500);
        c.record_frame(11, 2_000, 2_500);
        c.record_frame(14, 5_000, 5_500); // 12 and 13 were not delivered, with no reason given
        assert_eq!(c.frames(), 3);
        assert_eq!(c.drops(DropReason::Unattributed), 2);
        assert_eq!(c.last(), (14, 5_000));
    }

    #[test]
    fn a_drop_recorded_for_a_gap_explains_it_and_is_not_counted_twice() {
        let c = CameraCounters::new();
        c.record_frame(0, 1_000, 1_000);
        c.record_drop(DropReason::HeldBuffer);
        c.record_frame(2, 3_000, 3_000); // slot 1 was the held one
        assert_eq!(c.drops(DropReason::HeldBuffer), 1);
        assert_eq!(c.drops(DropReason::Unattributed), 0);
        // A drop with no gap after it does not explain the next gap.
        c.record_drop(DropReason::RingFull);
        c.record_frame(3, 4_000, 4_000);
        c.record_frame(5, 6_000, 6_000);
        assert_eq!(c.drops(DropReason::Unattributed), 1);
    }

    #[test]
    fn the_window_holds_min_median_max_and_the_rate_and_resets() {
        let c = CameraCounters::new();
        for i in 0..4_u64 {
            // Frames 1 ms apart, ages 2^20 .. 2^21 ns.
            let ts = 1_000_000 * (i + 1);
            c.record_frame(i, ts, ts + (1 << 20) + i * 100_000);
        }
        let w = c.take_window();
        assert_eq!(w.frames, 4);
        assert_eq!(w.min_age_ns, 1 << 20);
        assert_eq!(w.max_age_ns, (1 << 20) + 300_000);
        assert_eq!((w.first_id, w.first_ns), (0, 1_000_000));
        assert_eq!((w.last_id, w.last_ns), (3, 4_000_000));
        assert_eq!(achieved_fps_milli(0, 1_000_000, 3, 4_000_000), 1_000_000);
        // All four ages sit in bucket 20; its upper bound is capped at the window's max.
        let median = median_of(&w.hist, w.frames, w.max_age_ns);
        assert_eq!(median, w.max_age_ns);
        assert_eq!(w.hist[20], 4);
        let empty = c.take_window();
        assert_eq!(empty.frames, 0);
        assert_eq!(empty.min_age_ns, 0);
        assert_eq!(c.frames(), 4, "the total is not reset by a window");
    }

    #[test]
    fn buckets_are_powers_of_two_and_saturate() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1), 0);
        assert_eq!(bucket(2), 1);
        assert_eq!(bucket(1 << 20), 20);
        assert_eq!(bucket(u64::MAX), AGE_BUCKETS - 1);
    }
}
