//! Optional timing of the device calls (queue, dequeue, wait, config copies), for finding
//! where a frame's time goes on a device without `perf`. Off by default; a disabled section
//! costs one atomic load.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static ON: AtomicBool = AtomicBool::new(false);
static TABLE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// Totals of one section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Node or stage (e.g. `fe_stats`).
    pub what: &'static str,
    /// Operation (e.g. `dqbuf`, `poll`).
    pub op: &'static str,
    /// Calls.
    pub count: u64,
    /// Total time.
    pub total: Duration,
}

/// Turns timing on or off (and clears the totals when turning it on).
pub fn enable(on: bool) {
    if on {
        TABLE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    ON.store(on, Ordering::Relaxed);
}

/// Whether timing is on.
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Adds `elapsed` to section `(what, op)`.
pub fn record(what: &'static str, op: &'static str, elapsed: Duration) {
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    match t
        .iter_mut()
        .find(|e| std::ptr::eq(e.what, what) && std::ptr::eq(e.op, op))
    {
        Some(e) => {
            e.count += 1;
            e.total += elapsed;
        }
        None => t.push(Entry {
            what,
            op,
            count: 1,
            total: elapsed,
        }),
    }
}

/// Runs `f`, timing it as `(what, op)` when timing is on.
#[inline]
pub fn time<T>(what: &'static str, op: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let t = Instant::now();
    let r = f();
    record(what, op, t.elapsed());
    r
}

/// The totals so far, in first-use order.
pub fn report() -> Vec<Entry> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_add_up() {
        enable(true);
        time("x", "a", || ());
        time("x", "a", || ());
        record("x", "b", Duration::from_millis(2));
        let r = report();
        assert_eq!(r.iter().find(|e| e.op == "a").map(|e| e.count), Some(2));
        assert_eq!(
            r.iter().find(|e| e.op == "b").map(|e| e.total),
            Some(Duration::from_millis(2))
        );
        enable(false);
    }
}
