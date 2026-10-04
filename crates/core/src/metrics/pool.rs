use crate::sync::Counter;

/// Lightweight counters for pool/queue backpressure (relaxed atomics, `no_std`).
///
/// # Example
/// ```rust
/// use styx_core::metrics::Metrics;
///
/// let metrics = Metrics::default();
/// metrics.hit();
/// assert_eq!(metrics.hits(), 1);
/// ```
#[derive(Debug, Default, Clone)]
pub struct Metrics {
    hits: Counter,
    misses: Counter,
    allocations: Counter,
    backpressure: Counter,
    leases_out: Counter,
    peak_leases_out: Counter,
}

impl Metrics {
    /// Counters at zero.
    pub const fn new() -> Self {
        Self {
            hits: Counter::new(),
            misses: Counter::new(),
            allocations: Counter::new(),
            backpressure: Counter::new(),
            leases_out: Counter::new(),
            peak_leases_out: Counter::new(),
        }
    }

    /// Increment hit counter.
    #[inline]
    pub fn hit(&self) {
        self.hits.incr();
    }

    /// Increment miss counter.
    #[inline]
    pub fn miss(&self) {
        self.misses.incr();
    }

    /// Increment allocation counter.
    #[inline]
    pub fn alloc(&self) {
        self.allocations.incr();
    }

    /// Increment backpressure counter.
    #[inline]
    pub fn backpressure(&self) {
        self.backpressure.incr();
    }

    /// Record a checked-out pooled buffer.
    #[inline]
    pub fn lease_acquired(&self) {
        let current = self.leases_out.fetch_add(1);
        self.peak_leases_out.max(current.saturating_add(1));
    }

    /// Record a returned/dropped lease.
    #[inline]
    pub fn lease_released(&self) {
        self.leases_out.sub(1);
    }

    /// Snapshot of hits.
    pub fn hits(&self) -> u64 {
        self.hits.get()
    }

    /// Snapshot of misses.
    pub fn misses(&self) -> u64 {
        self.misses.get()
    }

    /// Snapshot of allocations.
    pub fn allocations(&self) -> u64 {
        self.allocations.get()
    }

    /// Snapshot of backpressure events.
    pub fn backpressure_count(&self) -> u64 {
        self.backpressure.get()
    }

    /// Snapshot of currently checked-out buffers.
    pub fn leases_out(&self) -> u64 {
        self.leases_out.get()
    }

    /// High-water mark of concurrently checked-out buffers.
    pub fn peak_leases_out(&self) -> u64 {
        self.peak_leases_out.get()
    }
}
