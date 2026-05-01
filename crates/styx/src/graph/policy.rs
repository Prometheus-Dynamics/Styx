/// Edge policy for preview/display branches: keep only the freshest frame.
pub fn preview_policy() -> daedalus::runtime::RuntimeEdgePolicy {
    daedalus::runtime::RuntimeEdgePolicy::latest_only()
}

/// Edge policy for recording branches where preserving order matters.
pub fn recording_policy(capacity: usize) -> daedalus::runtime::RuntimeEdgePolicy {
    daedalus::runtime::RuntimeEdgePolicy {
        pressure: daedalus::transport::PressurePolicy::Bounded {
            capacity: capacity.max(1),
            overflow: daedalus::transport::OverflowPolicy::Backpressure,
        },
        freshness: daedalus::transport::FreshnessPolicy::PreserveAll,
    }
}

/// Edge policy for analysis branches that should cap latency under load.
pub fn analysis_policy(
    capacity: usize,
    max_lag_frames: u64,
) -> daedalus::runtime::RuntimeEdgePolicy {
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
