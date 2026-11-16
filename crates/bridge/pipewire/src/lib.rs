//! A PipeWire video source for Styx frames: `Video/Source` node whose buffers are the producer's
//! memfds, so frames reach PipeWire consumers (`pipewiresrc`, browsers, the portal) without a copy.
//!
//! - [`link`]: the producer link. Connects, learns the buffers from the first frames each index
//!   carries, and reconnects with backoff (250 ms doubling to 5 s) when the producer goes away or
//!   comes back. Pure Rust, with the time passed in: tested with a fake clock.
//! - [`learn`]: takes each ring buffer's descriptor from the frames, one frame at a time.
//! - [`node`]: runs the PipeWire stream, in Rust on pipewire-rs (feature `pipewire`). Buffer `i` is
//!   ring buffer `i`; a frame is queued as that PipeWire buffer, held until PipeWire gives it back,
//!   so the producer cannot reuse it meanwhile. The node stays up while its producer is gone, and
//!   renegotiates the stream when a producer returns with new buffers.
//! - Without the `pipewire` feature the crate has no PipeWire dependency, and [`node::run`] returns
//!   [`node::NodeError::NotBuilt`].
//!
//! Frames in an index the node did not learn are dropped and counted; a frame whose consumer still
//! holds its buffer is released and counted busy (the producer drops, never waits). Linux only.
//!
//! `unsafe` appears only where PipeWire's C buffer structs are read and written, each block with a
//! `SAFETY` note.

#![cfg(target_os = "linux")]

#[cfg(any(feature = "pipewire", test))]
#[cfg_attr(not(feature = "pipewire"), allow(dead_code))]
mod hold;
pub mod learn;
pub mod link;
pub mod node;

pub use learn::{Buffers, LearnError, Learner};
pub use link::{Backoff, Link, LinkState, LinkStatus};
#[doc(hidden)]
pub use node::run_with_a_small_ring;
pub use node::{
    Control, MIN_BUFFERS, NodeConfig, NodeError, NodeStats, NodeStatus, node_built, run,
};
