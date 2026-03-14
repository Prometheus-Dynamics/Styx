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
    leases_out: AtomicU64,
    peak_leases_out: AtomicU64,
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

    /// Record a checked-out pooled buffer.
    pub fn lease_acquired(&self) {
        let current = self
            .leases_out
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.peak_leases_out.fetch_max(current, Ordering::Relaxed);
    }

    /// Record a returned/dropped lease.
    pub fn lease_released(&self) {
        self.leases_out.fetch_sub(1, Ordering::Relaxed);
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

    /// Snapshot of currently checked-out buffers.
    pub fn leases_out(&self) -> u64 {
        self.leases_out.load(Ordering::Relaxed)
    }

    /// High-water mark of concurrently checked-out buffers.
    pub fn peak_leases_out(&self) -> u64 {
        self.peak_leases_out.load(Ordering::Relaxed)
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
            .leases_out
            .store(self.leases_out(), Ordering::Relaxed);
        cloned
            .peak_leases_out
            .store(self.peak_leases_out(), Ordering::Relaxed);
        cloned
    }
}
