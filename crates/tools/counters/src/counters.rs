//! One camera's frame-path counters. Every update is an atomic operation: no lock, no allocation.
//!
//! One thread (the camera's) records frames; the publisher takes the window at its own low rate.
//! A take that races a frame puts that frame in this window or the next, never in both and never
//! in part: a window is always a whole run of frames (see [`Cell`] and
//! [`CameraCounters::take_window`]).

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
    /// How many times the publisher has asked for the window: the epoch a window is filled in.
    cut: AtomicU64,
    /// The window being filled, and the last one the camera's thread finished.
    open: Cell,
    done: Cell,
}

/// One window's numbers, written by the camera's thread alone and read whole by the publisher: a
/// single-writer seqlock, as [`Pair`], over all of them. So the count, the first and last frame
/// and the ages a reader gets are of the same frames.
#[derive(Debug)]
struct Cell {
    seq: AtomicU64,
    /// The epoch this window was started in (the value of `cut` then).
    epoch: AtomicU64,
    frames: AtomicU64,
    min_age_ns: AtomicU64,
    max_age_ns: AtomicU64,
    hist: [AtomicU32; AGE_BUCKETS],
    first_id: AtomicU64,
    first_ns: AtomicU64,
    last_id: AtomicU64,
    last_ns: AtomicU64,
}

impl Cell {
    fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            min_age_ns: AtomicU64::new(u64::MAX),
            max_age_ns: AtomicU64::new(0),
            hist: std::array::from_fn(|_| AtomicU32::new(0)),
            first_id: AtomicU64::new(0),
            first_ns: AtomicU64::new(0),
            last_id: AtomicU64::new(0),
            last_ns: AtomicU64::new(0),
        }
    }

    /// Opens a write: readers retry until [`Cell::end`]. Only the camera's thread writes.
    ///
    /// The mark is stored with a full barrier. The camera's thread reads the publisher's cut
    /// next, and the publisher reads this mark after moving the cut: one of the two must see the
    /// other's store, or a frame would go into a window the publisher has just taken without it.
    fn begin(&self) -> u64 {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::SeqCst);
        s
    }

    fn end(&self, s: u64) {
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// The window as it is now, with no write open. Called inside a write of the cell it is
    /// copied into, or by `read`.
    fn load(&self) -> (u64, Window) {
        (
            self.epoch.load(Ordering::Relaxed),
            Window {
                frames: self.frames.load(Ordering::Relaxed),
                min_age_ns: self.min_age_ns.load(Ordering::Relaxed),
                max_age_ns: self.max_age_ns.load(Ordering::Relaxed),
                hist: std::array::from_fn(|i| self.hist[i].load(Ordering::Relaxed)),
                first_id: self.first_id.load(Ordering::Relaxed),
                first_ns: self.first_ns.load(Ordering::Relaxed),
                last_id: self.last_id.load(Ordering::Relaxed),
                last_ns: self.last_ns.load(Ordering::Relaxed),
            },
        )
    }

    /// Stores `window` as the window of `epoch`. Called with a write open.
    fn store(&self, epoch: u64, window: &Window) {
        self.epoch.store(epoch, Ordering::Relaxed);
        self.frames.store(window.frames, Ordering::Relaxed);
        self.min_age_ns.store(window.min_age_ns, Ordering::Relaxed);
        self.max_age_ns.store(window.max_age_ns, Ordering::Relaxed);
        for (slot, n) in self.hist.iter().zip(window.hist) {
            slot.store(n, Ordering::Relaxed);
        }
        self.first_id.store(window.first_id, Ordering::Relaxed);
        self.first_ns.store(window.first_ns, Ordering::Relaxed);
        self.last_id.store(window.last_id, Ordering::Relaxed);
        self.last_ns.store(window.last_ns, Ordering::Relaxed);
    }

    /// The window and its epoch, never torn: a read that overlapped a write is made again.
    fn read(&self) -> (u64, Window) {
        loop {
            let before = self.seq.load(Ordering::SeqCst);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let read = self.load();
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return read;
            }
        }
    }
}

/// A window with no frame in it.
const EMPTY: Window = Window {
    frames: 0,
    min_age_ns: u64::MAX,
    max_age_ns: 0,
    hist: [0; AGE_BUCKETS],
    first_id: 0,
    first_ns: 0,
    last_id: 0,
    last_ns: 0,
};

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
            cut: AtomicU64::new(0),
            open: Cell::new(),
            done: Cell::new(),
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

        // The frame goes into the open window, in one write of it. The publisher's cut is read
        // inside that write: a take that has begun either sees this whole frame in the window it
        // takes (the write ended before its read), or its read is made again after this write,
        // which by then has seen the cut and put the frame in the next window.
        let age = now_ns.saturating_sub(timestamp_ns);
        let write = self.open.begin();
        let cut = self.cut.load(Ordering::SeqCst);
        let (epoch, mut window) = self.open.load();
        if epoch != cut {
            // The publisher asked for the window: the one being filled is finished. It is kept
            // whole for the publisher, and the next begins with this frame.
            let kept = self.done.begin();
            self.done.store(epoch, &window);
            self.done.end(kept);
            window = EMPTY;
        }
        if window.frames == 0 {
            window.first_id = id;
            window.first_ns = timestamp_ns;
        }
        window.frames += 1;
        window.last_id = id;
        window.last_ns = timestamp_ns;
        window.min_age_ns = window.min_age_ns.min(age);
        window.max_age_ns = window.max_age_ns.max(age);
        let slot = &mut window.hist[bucket(age)];
        *slot = slot.saturating_add(1);
        self.open.store(cut, &window);
        self.open.end(write);
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

    /// Takes the window and starts the next one. Called by the publisher, not the frame path, and
    /// by one thread at a time.
    ///
    /// The publisher asks for the window by moving `cut` on, then reads what the camera's thread
    /// was filling. If that is still the window of the epoch just ended, it is the finished
    /// window (the next frame will start a new one). If the camera's thread has already started
    /// the next, the finished one is the window it kept. A window of an older epoch was taken
    /// before: no frame has come since, and this window is empty.
    pub fn take_window(&self) -> Window {
        let ended = self.cut.fetch_add(1, Ordering::SeqCst);
        let (epoch, window) = self.open.read();
        let window = if epoch == ended {
            window
        } else if epoch > ended {
            match self.done.read() {
                (kept, window) if kept == ended => window,
                _ => EMPTY,
            }
        } else {
            EMPTY
        };
        Window {
            min_age_ns: if window.frames == 0 {
                0
            } else {
                window.min_age_ns
            },
            ..window
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

    /// The camera's thread records frames as fast as it can while the publisher takes windows as
    /// fast as it can: every window is a whole run of frames (its count is its last identity
    /// minus its first plus one, its times are those frames'), the windows follow one another
    /// with no frame between them, and together they hold every frame once.
    #[test]
    fn a_window_taken_while_frames_arrive_is_a_whole_run_of_frames() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        const FRAMES: u64 = 2_000_000;
        let counters = Arc::new(CameraCounters::new());
        let done = Arc::new(AtomicBool::new(false));
        let camera = {
            let (counters, done) = (counters.clone(), done.clone());
            std::thread::spawn(move || {
                for id in 1..=FRAMES {
                    counters.record_frame(id, id * 1_000, id * 1_000 + 7);
                }
                done.store(true, Ordering::Release);
            })
        };
        let (mut total, mut windows, mut last) = (0_u64, 0_u64, 0_u64);
        loop {
            let finished = done.load(Ordering::Acquire);
            let window = counters.take_window();
            if window.frames > 0 {
                windows += 1;
                assert_eq!(
                    window.first_id,
                    last + 1,
                    "window {windows} starts where the one before ended: {window:?}"
                );
                assert_eq!(
                    window.frames,
                    window.last_id - window.first_id + 1,
                    "window {windows}: its count is its run of identities: {window:?}"
                );
                assert_eq!(window.first_ns, window.first_id * 1_000, "{window:?}");
                assert_eq!(window.last_ns, window.last_id * 1_000, "{window:?}");
                assert_eq!((window.min_age_ns, window.max_age_ns), (7, 7), "{window:?}");
                assert_eq!(
                    window.hist.iter().map(|&n| u64::from(n)).sum::<u64>(),
                    window.frames,
                    "{window:?}"
                );
                total += window.frames;
                last = window.last_id;
            }
            if finished && window.frames == 0 {
                break;
            }
        }
        camera.join().expect("camera thread");
        assert_eq!(total, FRAMES, "every frame is in one window");
        println!(
            "proof: {FRAMES} frames in {windows} windows taken while they arrived, each whole"
        );
    }
}
