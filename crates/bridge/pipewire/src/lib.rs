//! A PipeWire video source for Styx frames: `Video/Source` node whose buffers are the producer's
//! memfds, so frames reach PipeWire consumers (`pipewiresrc`, browsers, the portal) without a copy.
//!
//! - [`link`]: the producer link. Connects, learns the buffers from the first frames each index
//!   carries, and reconnects with backoff (250 ms doubling to 5 s) when the producer goes away or
//!   comes back. Pure Rust, with the time passed in: tested with a fake clock.
//! - [`learn`]: takes each ring buffer's descriptor from the frames, one frame at a time.
//! - [`node`]: runs the PipeWire stream. The stream is C (`c_api/styx_pipewire.c`): SPA pods and
//!   the stream API are C-only. Buffer `i` is ring buffer `i`; a frame is queued as that PipeWire
//!   buffer, held until PipeWire gives it back, so the producer cannot reuse it meanwhile. The node
//!   stays up while its producer is gone, and renegotiates the stream when a producer returns with
//!   new buffers.
//! - The shim is built by `build.rs` when the `pipewire` feature is on and pkg-config finds
//!   libpipewire-0.3. Without it [`node::run`] returns [`node::NodeError::NotBuilt`].
//!
//! Frames in an index the node did not learn are dropped and counted; a frame whose consumer still
//! holds its buffer is released and counted busy (the producer drops, never waits). Linux only.
//!
//! `unsafe` appears only at the PipeWire boundary, each block with a `SAFETY` note.

#![cfg(target_os = "linux")]

pub mod learn;
pub mod link;
pub mod node;

pub use learn::{Buffers, LearnError, Learner};
pub use link::{Backoff, Link, LinkState, LinkStatus};
pub use node::{Control, NodeConfig, NodeError, NodeStats, NodeStatus, node_built, run};
