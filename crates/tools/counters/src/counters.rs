//! One camera's frame-path counters. Every update is an atomic operation: no lock, no allocation.
//!
//! One thread (the camera's) records frames; the publisher takes the window at its own low rate.
//! The frame path alone writes the window, and the publisher reads it whole: a window is one run of
//! consecutive frames, and a frame that races a take is in that window or the next, never in both
//! and never in part.

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

/// What the window held when it was taken. A window with no frame is all zero.
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

impl Window {
    /// A window no frame arrived in.
    pub const EMPTY: Self = Self {
        frames: 0,
        min_age_ns: 0,
        max_age_ns: 0,
        hist: [0; AGE_BUCKETS],
        first_id: 0,
        first_ns: 0,
        last_id: 0,
        last_ns: 0,
    };
}

/// One window's figures, and the take it belongs to (`tag`). Written by the frame path only, inside
/// a write of [`Windows::seq`].
#[derive(Debug)]
struct Cell {
    tag: AtomicU64,
    frames: AtomicU64,
    first_id: AtomicU64,
    first_ns: AtomicU64,
    last_id: AtomicU64,
    last_ns: AtomicU64,
    min_age: AtomicU64,
    max_age: AtomicU64,
    hist: [AtomicU32; AGE_BUCKETS],
}

impl Cell {
    fn new() -> Self {
        Self {
            tag: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            first_id: AtomicU64::new(0),
            first_ns: AtomicU64::new(0),
            last_id: AtomicU64::new(0),
            last_ns: AtomicU64::new(0),
            min_age: AtomicU64::new(0),
            max_age: AtomicU64::new(0),
            hist: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    /// Makes this the window `tag`, holding one frame.
    fn start(&self, tag: u64, id: u64, ns: u64, age: u64) {
        self.tag.store(tag, Ordering::Relaxed);
        self.frames.store(1, Ordering::Relaxed);
        self.first_id.store(id, Ordering::Relaxed);
        self.first_ns.store(ns, Ordering::Relaxed);
        self.last_id.store(id, Ordering::Relaxed);
        self.last_ns.store(ns, Ordering::Relaxed);
        self.min_age.store(age, Ordering::Relaxed);
        self.max_age.store(age, Ordering::Relaxed);
        let at = bucket(age);
        for (i, n) in self.hist.iter().enumerate() {
            n.store(u32::from(i == at), Ordering::Relaxed);
        }
    }

    /// Adds one frame. The frame path is the only writer, so each update is a load and a store.
    fn add(&self, id: u64, ns: u64, age: u64) {
        let frames = self.frames.load(Ordering::Relaxed);
        self.frames.store(frames + 1, Ordering::Relaxed);
        self.last_id.store(id, Ordering::Relaxed);
        self.last_ns.store(ns, Ordering::Relaxed);
        if age < self.min_age.load(Ordering::Relaxed) {
            self.min_age.store(age, Ordering::Relaxed);
        }
        if age > self.max_age.load(Ordering::Relaxed) {
            self.max_age.store(age, Ordering::Relaxed);
        }
        let n = &self.hist[bucket(age)];
        n.store(
            n.load(Ordering::Relaxed).saturating_add(1),
            Ordering::Relaxed,
        );
    }

    fn copy_from(&self, other: &Self) {
        let (tag, window) = other.read();
        self.tag.store(tag, Ordering::Relaxed);
        self.frames.store(window.frames, Ordering::Relaxed);
        self.first_id.store(window.first_id, Ordering::Relaxed);
        self.first_ns.store(window.first_ns, Ordering::Relaxed);
        self.last_id.store(window.last_id, Ordering::Relaxed);
        self.last_ns.store(window.last_ns, Ordering::Relaxed);
        self.min_age.store(window.min_age_ns, Ordering::Relaxed);
        self.max_age.store(window.max_age_ns, Ordering::Relaxed);
        for (to, from) in self.hist.iter().zip(window.hist) {
            to.store(from, Ordering::Relaxed);
        }
    }

    fn read(&self) -> (u64, Window) {
        let window = Window {
            frames: self.frames.load(Ordering::Relaxed),
            min_age_ns: self.min_age.load(Ordering::Relaxed),
            max_age_ns: self.max_age.load(Ordering::Relaxed),
            hist: std::array::from_fn(|i| self.hist[i].load(Ordering::Relaxed)),
            first_id: self.first_id.load(Ordering::Relaxed),
            first_ns: self.first_ns.load(Ordering::Relaxed),
            last_id: self.last_id.load(Ordering::Relaxed),
            last_ns: self.last_ns.load(Ordering::Relaxed),
        };
        (self.tag.load(Ordering::Relaxed), window)
    }
}

/// The frame windows of one camera: a single-writer seqlock over the window being filled and the
/// one before it.
///
/// The frame path is the only writer and never waits. The publisher never writes a window: a take
/// only asks for the next one (`asked`), and the frame path starts it with the first frame that
/// sees the request, keeping the window it leaves in `closed`. A take then reads, in one
/// consistent read, the window it ended: `open` if no frame has arrived since the request, else
/// `closed`. So a window's count, first and last frame and ages are always the same frames'.
///
/// A frame recorded while a take is in progress either finishes its write before the take's read
/// (the take reads it, in the window the frame chose) or starts it after the take's request (it
/// starts the next window). That needs the frame's "write begins, then read `asked`" and the
/// take's "write `asked`, then read the sequence" to be in one order for both threads, which is
/// what `SeqCst` on those four operations gives.
#[derive(Debug)]
struct Windows {
    seq: AtomicU64,
    /// The tag of the window frames now belong to. Starts at 1, and each take adds one.
    asked: AtomicU64,
    open: Cell,
    closed: Cell,
    /// The tag of the last window a take returned. Only the publisher reads and writes it.
    taken: AtomicU64,
}

impl Windows {
    fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            asked: AtomicU64::new(1),
            open: Cell::new(),
            closed: Cell::new(),
            taken: AtomicU64::new(0),
        }
    }

    /// Adds one frame to the window. Only the camera's thread calls this.
    fn record(&self, id: u64, ns: u64, age: u64) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::SeqCst);
        let asked = self.asked.load(Ordering::SeqCst);
        if self.open.tag.load(Ordering::Relaxed) == asked {
            self.open.add(id, ns, age);
        } else {
            self.closed.copy_from(&self.open);
            self.open.start(asked, id, ns, age);
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Ends the window and returns it. Only the publisher calls this, one take at a time.
    fn take(&self) -> Window {
        let asked = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let (tag, window) = loop {
            let before = self.seq.load(Ordering::SeqCst);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            // A frame that saw this take's request has already started the next window: the one
            // this take ended is the closed one.
            let ended = if self.open.tag.load(Ordering::Relaxed) == asked {
                &self.closed
            } else {
                &self.open
            };
            let read = ended.read();
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                break read;
            }
        };
        // No frame since the last take: the window read is the one that take returned.
        if tag <= self.taken.load(Ordering::Relaxed) {
            return Window::EMPTY;
        }
        self.taken.store(tag, Ordering::Relaxed);
        window
    }
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
    windows: Windows,
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
            windows: Windows::new(),
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

        self.windows
            .record(id, timestamp_ns, now_ns.saturating_sub(timestamp_ns));
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

    /// Takes the window and starts the next one: the frames delivered since the last take, as one
    /// run. Called by the publisher (one take at a time), not the frame path.
    pub fn take_window(&self) -> Window {
        self.windows.take()
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
        assert_eq!(c.take_window(), Window::EMPTY, "no frame since the take");
        assert_eq!(c.frames(), 4, "the total is not reset by a window");
    }

    /// The publisher takes windows while the camera's thread records frames, as fast as both can.
    /// Every window is one run of consecutive frames: its count, its first and last frame and its
    /// ages are the same frames', and the windows follow each other with no frame lost or counted
    /// twice.
    #[test]
    fn a_window_taken_while_frames_arrive_is_one_run_of_frames() {
        const FRAMES: u64 = 500_000;
        const STEP_NS: u64 = 1_000;
        let c = std::sync::Arc::new(CameraCounters::new());
        let writer = {
            let c = std::sync::Arc::clone(&c);
            std::thread::spawn(move || {
                for i in 0..FRAMES {
                    let ts = (i + 1) * STEP_NS;
                    c.record_frame(i, ts, ts + (i & 0xFFFF));
                }
            })
        };
        let mut next = 0_u64;
        let mut check = |w: Window| {
            if w.frames == 0 {
                assert_eq!((w.first_id, w.first_ns, w.last_id, w.last_ns), (0, 0, 0, 0));
                assert_eq!((w.min_age_ns, w.max_age_ns), (0, 0));
                return;
            }
            assert_eq!(
                w.first_id, next,
                "the window starts where the last one ended"
            );
            assert_eq!(w.last_id - w.first_id + 1, w.frames, "{w:?}");
            assert_eq!(w.first_ns, (w.first_id + 1) * STEP_NS, "{w:?}");
            assert_eq!(w.last_ns, (w.last_id + 1) * STEP_NS, "{w:?}");
            let counted: u64 = w.hist.iter().map(|n| u64::from(*n)).sum();
            assert_eq!(counted, w.frames, "{w:?}");
            let ages = (w.first_id..=w.last_id).map(|i| i & 0xFFFF);
            assert_eq!(w.min_age_ns, ages.clone().min().unwrap_or(0), "{w:?}");
            assert_eq!(w.max_age_ns, ages.max().unwrap_or(0), "{w:?}");
            if w.frames >= 2 {
                let rate = achieved_fps_milli(w.first_id, w.first_ns, w.last_id, w.last_ns);
                assert_eq!(rate, 1_000_000_000, "{w:?}");
            }
            next = w.last_id + 1;
        };
        while !writer.is_finished() {
            check(c.take_window());
        }
        writer.join().expect("the frame thread");
        check(c.take_window());
        // Nothing arrived since: the next window is empty.
        check(c.take_window());
        assert_eq!(next, FRAMES, "every frame was in exactly one window");
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
