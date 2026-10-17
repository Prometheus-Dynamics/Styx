//! A frame handed out while its buffers are still being written, top to bottom (Styx's sliced
//! native frames): how many rows of each output are final so far, and whether the frame
//! completed or failed. A frame without progress ([`super::FrameLease::progress`] `None`) is
//! complete.
//!
//! Rows at or above the published count are final and cache-coherent for the CPU; rows below it
//! may be stale or half written and must not be read until a later count covers them.
//!
//! A frame from another process (a camera service's sliced frame) reads its progress from a
//! [`ProgressSource`] the receiver provides, on every query.

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
    #[cfg(feature = "std")]
    source: Option<Source>,
}

/// How far a frame has come, as a [`ProgressSource`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    /// Being written: the rows reported are final.
    Running,
    /// Every row is final.
    Complete,
    /// It will not complete.
    Failed,
}

/// Where a frame's progress comes from when something else (another process) writes it: read
/// on every query of the frame's [`FrameProgressRef`]s, waited on by their waits.
#[cfg(feature = "std")]
pub trait ProgressSource: Send + Sync {
    /// Rows final so far per output, and the state.
    fn read(&self) -> ([u32; 2], ProgressState);
    /// Blocks up to `timeout` or until the progress may have changed (returning early is
    /// fine: the caller reads again).
    fn wait(&self, timeout: std::time::Duration);
}

#[cfg(feature = "std")]
struct Source(alloc::sync::Arc<dyn ProgressSource>);

#[cfg(feature = "std")]
impl core::fmt::Debug for Source {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ProgressSource")
    }
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
            #[cfg(feature = "std")]
            source: None,
        }
    }

    /// A frame whose progress `source` reports (it is read on every query).
    #[cfg(feature = "std")]
    pub fn with_source(source: alloc::sync::Arc<dyn ProgressSource>) -> Self {
        let mut p = Self::new([0, 0]);
        p.source = Some(Source(source));
        p.refresh();
        p
    }

    /// Takes the source's latest report, if there is a source.
    fn refresh(&self) {
        #[cfg(feature = "std")]
        if let Some(Source(source)) = &self.source {
            let (rows, state) = source.read();
            for (a, r) in self.rows.iter().zip(rows) {
                a.fetch_max(r, Ordering::Release);
            }
            match state {
                ProgressState::Running => {}
                ProgressState::Complete => self.state.store(COMPLETE, Ordering::Release),
                ProgressState::Failed => {
                    let _ = self.state.compare_exchange(
                        RUNNING,
                        FAILED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
            }
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
        self.progress.refresh();
        self.progress.rows[self.output].load(Ordering::Acquire)
    }

    /// Every row is final.
    pub fn is_complete(&self) -> bool {
        self.progress.refresh();
        self.progress.state.load(Ordering::Acquire) == COMPLETE
    }

    /// The frame will not complete.
    pub fn is_failed(&self) -> bool {
        self.progress.refresh();
        self.progress.state.load(Ordering::Acquire) == FAILED
    }

    /// Which of the frame's outputs this is (0: the main output, 1: the second).
    pub fn output(&self) -> usize {
        self.output
    }

    /// Rows final so far of each of the frame's outputs (this lease's and its companions').
    pub fn all_rows(&self) -> [u32; 2] {
        self.progress.refresh();
        [0, 1].map(|i| self.progress.rows[i].load(Ordering::Acquire))
    }

    /// The frame's state now.
    pub fn state(&self) -> ProgressState {
        self.progress.refresh();
        match self.progress.state.load(Ordering::Acquire) {
            COMPLETE => ProgressState::Complete,
            FAILED => ProgressState::Failed,
            _ => ProgressState::Running,
        }
    }

    /// Without waiting: what [`Self::wait`] would return now, `None` while fewer than
    /// `at_least` rows are final and the frame runs (for builds without threads, too).
    pub fn ready(&self, at_least: u32) -> Option<ProgressWait> {
        self.progress.refresh();
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
        if let Some(Source(source)) = &self.progress.source {
            loop {
                if let Some(w) = self.ready(at_least) {
                    return w;
                }
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return ProgressWait::TimedOut;
                }
                source.wait(left);
            }
        }
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
        assert_eq!(p.output(1).output(), 1);
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

    /// A source written elsewhere (another process): read on every query, waited on.
    #[test]
    fn a_source_reports_progress_written_elsewhere() {
        use std::sync::Mutex;
        struct Shared(Mutex<([u32; 2], ProgressState)>, std::sync::Condvar);
        impl ProgressSource for Shared {
            fn read(&self) -> ([u32; 2], ProgressState) {
                *self.0.lock().unwrap()
            }
            fn wait(&self, timeout: Duration) {
                let g = self.0.lock().unwrap();
                let _ = self.1.wait_timeout(g, timeout).unwrap();
            }
        }
        let shared = Arc::new(Shared(
            Mutex::new(([552, 274], ProgressState::Running)),
            std::sync::Condvar::new(),
        ));
        let p = Arc::new(FrameProgress::with_source(shared.clone()));
        assert_eq!((p.output(0).rows(), p.output(1).rows()), (552, 274));
        assert_eq!(p.output(0).ready(600), None);
        let s2 = shared.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            *s2.0.lock().unwrap() = ([800, 400], ProgressState::Complete);
            s2.1.notify_all();
        });
        assert_eq!(
            p.output(0).wait_complete(Duration::from_secs(5)),
            ProgressWait::Complete
        );
        t.join().unwrap();
        assert_eq!(p.output(1).rows(), 400);
        let failed = FrameProgress::with_source(Arc::new(Shared(
            Mutex::new(([0, 0], ProgressState::Failed)),
            std::sync::Condvar::new(),
        )));
        assert!(failed.output(0).is_failed());
    }
}
