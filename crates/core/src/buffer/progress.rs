//! A frame handed out while its buffers are still being written, top to bottom (Styx's sliced
//! native frames): how many rows of each output are final so far, and whether the frame
//! completed or failed. A frame without progress ([`super::FrameLease::progress`] `None`) is
//! complete.
//!
//! Rows at or above the published count are final and cache-coherent for the CPU; rows below it
//! may be stale or half written and must not be read until a later count covers them.

use crate::sync::{AtomicU8, AtomicU32, Ordering};

const RUNNING: u8 = 0;
const COMPLETE: u8 = 1;
const FAILED: u8 = 2;

/// Rows written so far per output of a frame in progress, shared by the frame's leases (the
/// main output's and its companions').
#[derive(Debug)]
pub struct FrameProgress {
    rows: [AtomicU32; 2],
    state: AtomicU8,
    #[cfg(feature = "std")]
    lock: parking_lot::Mutex<()>,
    #[cfg(feature = "std")]
    changed: parking_lot::Condvar,
}

/// What a wait for rows ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressWait {
    /// At least the rows asked for are final (the count published).
    Rows(u32),
    /// The frame is complete: every row is final.
    Complete,
    /// The frame failed: it will not complete (drop it).
    Failed,
    /// Neither within the timeout.
    TimedOut,
}

impl FrameProgress {
    /// A frame whose outputs have `rows` rows final.
    pub fn new(rows: [u32; 2]) -> Self {
        Self {
            rows: [AtomicU32::new(rows[0]), AtomicU32::new(rows[1])],
            state: AtomicU8::new(RUNNING),
            #[cfg(feature = "std")]
            lock: parking_lot::Mutex::new(()),
            #[cfg(feature = "std")]
            changed: parking_lot::Condvar::new(),
        }
    }

    /// More rows are final (counts only grow).
    pub fn publish(&self, rows: [u32; 2]) {
        for (a, r) in self.rows.iter().zip(rows) {
            a.fetch_max(r, Ordering::Release);
        }
        self.notify();
    }

    /// Every row is final: `rows` is each output's height.
    pub fn complete(&self, rows: [u32; 2]) {
        for (a, r) in self.rows.iter().zip(rows) {
            a.fetch_max(r, Ordering::Release);
        }
        self.state.store(COMPLETE, Ordering::Release);
        self.notify();
    }

    /// The frame will not complete.
    pub fn fail(&self) {
        self.state.store(FAILED, Ordering::Release);
        self.notify();
    }

    fn notify(&self) {
        #[cfg(feature = "std")]
        {
            let _g = self.lock.lock();
            self.changed.notify_all();
        }
    }

    /// Output `output`'s view.
    pub fn output(&self, output: usize) -> FrameProgressRef<'_> {
        FrameProgressRef {
            progress: self,
            output: output.min(1),
        }
    }
}

/// One output's progress (a lease's: the main output's or a companion's).
#[derive(Clone, Copy, Debug)]
pub struct FrameProgressRef<'a> {
    progress: &'a FrameProgress,
    output: usize,
}

impl FrameProgressRef<'_> {
    /// Rows final so far (from the top).
    pub fn rows(&self) -> u32 {
        self.progress.rows[self.output].load(Ordering::Acquire)
    }

    /// Every row is final.
    pub fn is_complete(&self) -> bool {
        self.progress.state.load(Ordering::Acquire) == COMPLETE
    }

    /// The frame will not complete.
    pub fn is_failed(&self) -> bool {
        self.progress.state.load(Ordering::Acquire) == FAILED
    }

    /// Without waiting: what [`Self::wait`] would return now, `None` while fewer than
    /// `at_least` rows are final and the frame runs (for builds without threads, too).
    pub fn ready(&self, at_least: u32) -> Option<ProgressWait> {
        match self.progress.state.load(Ordering::Acquire) {
            COMPLETE => Some(ProgressWait::Complete),
            FAILED => Some(ProgressWait::Failed),
            _ => {
                let r = self.rows();
                (r >= at_least).then_some(ProgressWait::Rows(r))
            }
        }
    }

    /// Waits up to `timeout` until at least `at_least` rows are final, or the frame completes
    /// or fails.
    #[cfg(feature = "std")]
    pub fn wait(&self, at_least: u32, timeout: std::time::Duration) -> ProgressWait {
        let deadline = std::time::Instant::now() + timeout;
        let mut g = self.progress.lock.lock();
        loop {
            if let Some(w) = self.ready(at_least) {
                return w;
            }
            if self
                .progress
                .changed
                .wait_until(&mut g, deadline)
                .timed_out()
            {
                return self.ready(at_least).unwrap_or(ProgressWait::TimedOut);
            }
        }
    }

    /// Waits up to `timeout` for the frame to complete (or fail).
    #[cfg(feature = "std")]
    pub fn wait_complete(&self, timeout: std::time::Duration) -> ProgressWait {
        self.wait(u32::MAX, timeout)
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn rows_grow_and_waiters_wake() {
        let p = Arc::new(FrameProgress::new([0, 0]));
        let (main, level) = (p.output(0), p.output(1));
        assert_eq!((main.rows(), main.is_complete()), (0, false));
        assert_eq!(
            main.wait(1, Duration::from_millis(5)),
            ProgressWait::TimedOut
        );
        p.publish([592, 294]);
        assert_eq!(main.wait(500, Duration::ZERO), ProgressWait::Rows(592));
        assert_eq!(level.rows(), 294);
        // Counts never go back.
        p.publish([100, 100]);
        assert_eq!(main.rows(), 592);
        let p2 = p.clone();
        let t = std::thread::spawn(move || p2.output(0).wait_complete(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(10));
        p.complete([800, 400]);
        assert_eq!(t.join().unwrap(), ProgressWait::Complete);
        assert_eq!((main.rows(), level.rows()), (800, 400));
        let f = FrameProgress::new([0, 0]);
        f.fail();
        assert_eq!(
            f.output(0).wait(10, Duration::from_secs(1)),
            ProgressWait::Failed
        );
    }
}
