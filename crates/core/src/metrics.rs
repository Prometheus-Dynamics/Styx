//! Counters, `no_std`, relaxed atomics only (no locks, no allocation on the frame path):
//!
//! * [`Metrics`]: a pool's or a queue's hits, misses, allocations, backpressure and leases.
//! * [`Counters`] and the rest of the camera counters: frames, drops by cause, rings of
//!   latencies and ISP times, the 3A loop's and AF's state, stills. `styx_runtime::metrics`
//!   is this module (re-exported): one set of counters from the receiver to `styx::metrics`.
//! * [`Counter`], [`Ring`]: the primitives both are built from.

mod camera;
mod pool;

pub use crate::sync::Counter;
pub use camera::*;
pub use pool::Metrics;
