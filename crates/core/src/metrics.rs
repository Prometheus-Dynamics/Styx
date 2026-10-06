//! Counters, `no_std`, relaxed atomics only (no locks, no allocation on the frame path):
//!
//! * [`Metrics`]: a pool's or a queue's hits, misses, allocations, backpressure and leases.
//! * [`Counters`] and the rest of the camera counters: frames, drops by cause, rings of
//!   latencies and ISP times, the 3A loop's and AF's state, stills. `styx_runtime::metrics`
//!   is this module (re-exported): one set of counters from the receiver to `styx::metrics`.
//! * [`HopCounters`], [`copied`], [`synced`], [`path_counters`] (`path-metrics` feature, on
//!   with `std`): windows of the time between a frame's hops, and the copies, dma-buf syncs
//!   and exhausted pools of the frame path.
//! * [`Counter`], [`Ring`]: the primitives both are built from.

mod camera;
#[cfg(feature = "path-metrics")]
mod path;
mod pool;

pub use crate::sync::Counter;
pub use camera::*;
#[cfg(feature = "path-metrics")]
pub use path::*;
pub use pool::Metrics;
