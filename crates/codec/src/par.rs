//! Row-parallel loops for the CPU converters, on one persistent pool of helper threads.
//!
//! The helpers start on first use and sleep between frames, so a frame costs a wake-up, not a
//! spawn (the design of `styx-softisp`'s pool). One loop runs on the pool at a time: a loop
//! that finds it busy (another stream converting, or a loop nested in a row) runs on its own
//! thread, which keeps every core busy without oversubscribing them.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, TryLockError};

/// Threads per loop, the caller included: memory bandwidth, not cores, bounds these loops.
const MAX_THREADS: usize = 8;
/// Fewer rows than this per band are not worth a wake-up.
const MIN_ROWS_PER_BAND: usize = 32;

/// Runs `f(y, row)` for every `row_bytes`-sized row of `dst` (the last one may be shorter),
/// rows in parallel bands. Like `dst.chunks_mut(row_bytes).enumerate().for_each(..)`.
pub(crate) fn for_each_row(dst: &mut [u8], row_bytes: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    if row_bytes == 0 || dst.is_empty() {
        return;
    }
    let rows = dst.len().div_ceil(row_bytes);
    let serial = |dst: &mut [u8]| {
        for (y, row) in dst.chunks_mut(row_bytes).enumerate() {
            f(y, row);
        }
    };
    let want = (rows / MIN_ROWS_PER_BAND).min(MAX_THREADS);
    if want <= 1 {
        return serial(dst);
    }
    let Some(pool) = pool() else {
        return serial(dst);
    };
    let guard = match pool.busy.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
        Err(TryLockError::WouldBlock) => return serial(dst),
    };
    let bands = want.min(pool.helpers() + 1);
    let rows_per_band = rows.div_ceil(bands);
    let parts: Vec<Mutex<&mut [u8]>> = dst
        .chunks_mut(rows_per_band * row_bytes)
        .map(Mutex::new)
        .collect();
    pool.run(parts.len(), &|band| {
        let mut part = parts[band].lock().unwrap_or_else(|e| e.into_inner());
        for (k, row) in part.chunks_mut(row_bytes).enumerate() {
            f(band * rows_per_band + k, row);
        }
    });
    drop(guard);
}

/// Runs `f` once on every helper thread of the pool (if it has started) and on the caller.
#[cfg(feature = "image")]
pub(crate) fn broadcast(f: impl Fn() + Sync) {
    f();
    let Some(pool) = POOL.get().and_then(Option::as_ref) else {
        return;
    };
    let _guard = pool.busy.lock().unwrap_or_else(|e| e.into_inner());
    let n = pool.helpers() + 1;
    pool.run(n, &|i| {
        if i != 0 {
            f();
        }
    });
}

static POOL: OnceLock<Option<Pool>> = OnceLock::new();

fn pool() -> Option<&'static Pool> {
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let helpers = threads.min(MAX_THREADS).saturating_sub(1);
        (helpers > 0).then(|| Pool::new(helpers))
    })
    .as_ref()
}

type Job = dyn Fn(usize) + Sync + 'static;

/// The job of the current round, its lifetime erased.
#[derive(Clone, Copy)]
struct JobPtr(*const Job);

// SAFETY: the pointee is `Sync`, and `Pool::run` keeps it alive until every helper that took
// it has finished with it.
unsafe impl Send for JobPtr {}

#[derive(Default)]
struct State {
    /// Round counter; helpers wait for it to change.
    round: u64,
    job: Option<JobPtr>,
    /// Helpers taking part in the round (helpers `1..=active`).
    active: usize,
    /// Helpers of the round still running.
    pending: usize,
    panicked: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    start: Condvar,
    done: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Jobs run outside the lock, so a poisoned lock only means a panic elsewhere.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Helper threads `1..=helpers`; the calling thread is index 0. Lives for the process.
struct Pool {
    shared: Arc<Shared>,
    helpers: usize,
    /// Held by the loop running on the pool.
    busy: Mutex<()>,
}

impl Pool {
    /// Starts `helpers` threads (fewer if the system refuses to start more).
    fn new(helpers: usize) -> Self {
        let shared = Arc::new(Shared::default());
        let started = (1..=helpers)
            .map_while(|i| {
                let s = shared.clone();
                std::thread::Builder::new()
                    .name(format!("styx-codec-{i}"))
                    .spawn(move || helper(&s, i))
                    .ok()
            })
            .count();
        Self {
            shared,
            helpers: started,
            busy: Mutex::new(()),
        }
    }

    fn helpers(&self) -> usize {
        self.helpers
    }

    /// Runs `f(i)` for every `i` in `0..n` (`n <= helpers + 1`), `f(0)` on the calling thread,
    /// and returns when all have finished. A panic in a helper is resumed here. The caller
    /// holds `busy`.
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync + '_)) {
        let n = n.min(self.helpers() + 1);
        if n <= 1 {
            if n == 1 {
                f(0);
            }
            return;
        }
        // SAFETY: only the lifetime is erased; `Wait` below does not return (or unwind past
        // this frame) before every helper of this round has finished calling the job and the
        // pointer has been cleared.
        let job =
            JobPtr(unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync + '_), &'static Job>(f) });
        {
            let mut s = self.shared.lock();
            s.round += 1;
            s.job = Some(job);
            s.active = n - 1;
            s.pending = n - 1;
        }
        self.shared.start.notify_all();

        struct Wait<'a>(&'a Shared);
        impl Drop for Wait<'_> {
            fn drop(&mut self) {
                let mut s = self.0.lock();
                while s.pending > 0 {
                    s = self.0.done.wait(s).unwrap_or_else(|e| e.into_inner());
                }
                s.job = None;
            }
        }
        let wait = Wait(&self.shared);
        f(0);
        drop(wait);
        let mut s = self.shared.lock();
        if std::mem::take(&mut s.panicked) {
            drop(s);
            panic!("a styx-codec worker thread panicked");
        }
    }
}

fn helper(shared: &Shared, index: usize) {
    let mut seen = 0;
    loop {
        let job = {
            let mut s = shared.lock();
            while s.round == seen {
                s = shared.start.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            seen = s.round;
            if index > s.active {
                continue;
            }
            s.job.expect("a round has a job")
        };
        // SAFETY: `Pool::run` keeps the job alive until `pending` reaches zero below.
        let r = catch_unwind(AssertUnwindSafe(|| unsafe { (*job.0)(index) }));
        let mut s = shared.lock();
        s.panicked |= r.is_err();
        s.pending -= 1;
        if s.pending == 0 {
            shared.done.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn every_row_once_with_its_index() {
        for (len, row_bytes) in [
            (0, 3),
            (5, 3),
            (3 * 1000, 3),
            (3 * 1000 + 2, 3),
            (7 * 4099, 7),
        ] {
            let mut dst = vec![0u8; len];
            let calls = AtomicUsize::new(0);
            for_each_row(&mut dst, row_bytes, |y, row| {
                calls.fetch_add(1, Ordering::Relaxed);
                row.fill((y % 251) as u8 + 1);
            });
            assert_eq!(calls.load(Ordering::Relaxed), len.div_ceil(row_bytes));
            for (y, row) in dst.chunks(row_bytes).enumerate() {
                assert!(row.iter().all(|&b| b == (y % 251) as u8 + 1), "row {y}");
            }
        }
    }

    #[test]
    fn nested_and_concurrent_loops_finish() {
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    let mut outer = vec![0u8; 64 * 512];
                    for_each_row(&mut outer, 512, |_, row| {
                        for_each_row(row, 4, |_, px| px.fill(1));
                    });
                    assert!(outer.iter().all(|&b| b == 1));
                });
            }
        });
    }

    #[test]
    fn a_panicking_row_reaches_the_caller_and_the_pool_recovers() {
        let r = catch_unwind(AssertUnwindSafe(|| {
            let mut dst = vec![0u8; 4096 * 4];
            for_each_row(&mut dst, 4, |y, _| assert_ne!(y, 4000, "boom"));
        }));
        assert!(r.is_err());
        let mut dst = vec![0u8; 4096 * 4];
        for_each_row(&mut dst, 4, |_, row| row.fill(2));
        assert!(dst.iter().all(|&b| b == 2));
    }
}
