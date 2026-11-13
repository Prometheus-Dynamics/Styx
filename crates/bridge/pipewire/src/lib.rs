//! A PipeWire video source for Styx frames: `Video/Source` node whose buffers are the producer's
//! memfds, so frames reach PipeWire consumers (`pipewiresrc`, browsers, the portal) without a copy.
//!
//! - [`learn`]: the node learns each ring buffer's memfd from the first frames in it (a reader
//!   sees a buffer's descriptor only through a frame), then releases those frames. Pure Rust.
//! - [`node`]: runs the PipeWire stream. The stream is C (`c_api/styx_pipewire.c`): SPA pods and
//!   the stream API are C-only. Buffer `i` is ring buffer `i`; a frame is queued as that PipeWire
//!   buffer, held until PipeWire gives it back, so the producer cannot reuse it meanwhile.
//! - The shim is built by `build.rs` when the `pipewire` feature is on and pkg-config finds
//!   libpipewire-0.3. Without it [`node::run`] returns [`node::NodeError::NotBuilt`].
//!
//! Frames in an index the node did not learn are dropped and counted; a frame whose consumer still
//! holds its buffer is released and counted busy (the producer drops, never waits). Linux only.
//!
//! `unsafe` appears only at the PipeWire boundary, each block with a `SAFETY` note.

#![cfg(target_os = "linux")]

pub mod learn;
pub mod node;

pub use learn::{Buffers, LearnError, connect, learn};
pub use node::{NodeConfig, NodeError, NodeStats, node_built, run};
