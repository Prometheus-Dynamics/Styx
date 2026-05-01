pub type GraphPolicy = daedalus::runtime::RuntimeEdgePolicy;
pub type SinkPolicy = GraphPolicy;

/// Edge policy for consumers that only need the freshest frame.
pub fn latest_only() -> GraphPolicy {
    daedalus::runtime::RuntimeEdgePolicy::latest_only()
}

/// Edge policy for bounded consumers where preserving order matters.
pub fn bounded_blocking(capacity: usize) -> GraphPolicy {
    daedalus::runtime::RuntimeEdgePolicy {
        pressure: daedalus::transport::PressurePolicy::Bounded {
            capacity: capacity.max(1),
            overflow: daedalus::transport::OverflowPolicy::Backpressure,
        },
        freshness: daedalus::transport::FreshnessPolicy::PreserveAll,
    }
}

/// Edge policy for bounded consumers that should cap latency under load.
pub fn bounded_drop_oldest(capacity: usize, max_lag_frames: u64) -> GraphPolicy {
    daedalus::runtime::RuntimeEdgePolicy {
        pressure: daedalus::transport::PressurePolicy::Bounded {
            capacity: capacity.max(1),
            overflow: daedalus::transport::OverflowPolicy::DropOldest,
        },
        freshness: daedalus::transport::FreshnessPolicy::MaxLag {
            frames: max_lag_frames.max(1),
        },
    }
}
