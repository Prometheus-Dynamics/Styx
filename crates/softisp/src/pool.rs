//! A persistent pool of helper threads for row bands: threads are started once per
//! [`crate::SoftIsp`] and sleep between frames, so a frame costs a wake-up, not a spawn.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

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
    shutdown: bool,
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

/// Helper threads `1..=helpers`; the calling thread is index 0.
pub(crate) struct Pool {
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("helpers", &self.handles.len())
            .finish()
    }
}

impl Pool {
    /// Starts `helpers` threads (fewer if the system refuses to start more).
    pub fn new(helpers: usize) -> Self {
        let shared = Arc::new(Shared::default());
        let handles = (1..=helpers)
            .map_while(|i| {
                let s = shared.clone();
                std::thread::Builder::new()
                    .name(format!("styx-softisp-{i}"))
                    .spawn(move || helper(&s, i))
                    .ok()
            })
            .collect();
        Self { shared, handles }
    }

    pub fn helpers(&self) -> usize {
        self.handles.len()
    }

    /// Runs `f(i)` for every `i` in `0..n` (`n <= helpers + 1`), `f(0)` on the calling thread,
    /// and returns when all have finished. A panic in a helper is resumed here.
    pub fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync + '_)) {
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
            panic!("a software ISP worker thread panicked");
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.start.notify_all();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

fn helper(shared: &Shared, index: usize) {
    let mut seen = 0;
    loop {
        let job = {
            let mut s = shared.lock();
            while s.round == seen && !s.shutdown {
                s = shared.start.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            if s.shutdown {
                return;
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
    fn runs_every_index_once_per_round() {
        let pool = Pool::new(3);
        for n in [1, 2, 4, 3, 4] {
            let hits: Vec<AtomicUsize> = (0..4).map(|_| AtomicUsize::new(0)).collect();
            pool.run(n, &|i| {
                hits[i].fetch_add(1, Ordering::Relaxed);
            });
            let counts: Vec<usize> = hits.iter().map(|h| h.load(Ordering::Relaxed)).collect();
            let want: Vec<usize> = (0..4).map(|i| usize::from(i < n)).collect();
            assert_eq!(counts, want, "n = {n}");
        }
    }

    #[test]
    fn helper_panic_reaches_the_caller_and_the_pool_recovers() {
        let pool = Pool::new(2);
        let r = catch_unwind(AssertUnwindSafe(|| {
            pool.run(3, &|i| assert_ne!(i, 2, "boom"));
        }));
        assert!(r.is_err());
        let hits = AtomicUsize::new(0);
        pool.run(3, &|_| {
            hits.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(hits.load(Ordering::Relaxed), 3);
    }
}
