use std::sync::atomic::{AtomicU64, Ordering};

/// Lightweight counters for pool/queue backpressure.
///
/// # Example
/// ```rust
/// use styx_core::metrics::Metrics;
///
/// let metrics = Metrics::default();
/// metrics.hit();
/// assert_eq!(metrics.hits(), 1);
/// ```
#[derive(Debug, Default)]
pub struct Metrics {
    hits: AtomicU64,
    misses: AtomicU64,
    allocations: AtomicU64,
    backpressure: AtomicU64,
}

impl Metrics {
    /// Increment hit counter.
    pub fn hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment miss counter.
    pub fn miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment allocation counter.
    pub fn alloc(&self) {
        self.allocations.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment backpressure counter.
    pub fn backpressure(&self) {
        self.backpressure.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of hits.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Snapshot of misses.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Snapshot of allocations.
    pub fn allocations(&self) -> u64 {
        self.allocations.load(Ordering::Relaxed)
    }

    /// Snapshot of backpressure events.
    pub fn backpressure_count(&self) -> u64 {
        self.backpressure.load(Ordering::Relaxed)
    }
}

impl Clone for Metrics {
    fn clone(&self) -> Self {
        let cloned = Metrics::default();
        cloned.hits.store(self.hits(), Ordering::Relaxed);
        cloned.misses.store(self.misses(), Ordering::Relaxed);
        cloned
            .allocations
            .store(self.allocations(), Ordering::Relaxed);
        cloned
            .backpressure
            .store(self.backpressure_count(), Ordering::Relaxed);
        cloned
    }
}
