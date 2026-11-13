//! Multi-camera frame grouping by timestamp (`no_std` + `alloc`).
//!
//! A [`Grouper`] takes frames from up to `N` cameras, each stamped on one clock (for Styx frames,
//! `FrameLease::timestamp_ns`, CLOCK_MONOTONIC), and hands out [`FrameGroup`]s: one frame per
//! camera whose timestamps lie within [`GroupConfig::tolerance_ns`] of the group's anchor.
//!
//! - **Bounded.** A camera holds at most [`GroupConfig::depth`] pending frames, and at most
//!   [`GroupConfig::output_depth`] groups wait to be taken. Everything past that is dropped, never
//!   copied, so a frame pool behind it never starves. Storage is made by [`Grouper::new`] (and one
//!   lane per camera); pushing, polling and popping allocate nothing.
//! - **Counted.** Every frame that leaves without being grouped is counted by [`DropReason`], per
//!   camera. [`Stats`] keeps the rest: frames received and grouped, groups by kind, spread.
//! - **Pure.** The grouper never reads a clock. The caller passes when each frame arrived and the
//!   time now, on one monotonic timeline, for deadlines; it polls again at [`Grouper::next_wake`].
//!
//! The matching:
//!
//! - **Anchor.** Each step takes an anchor frame: the oldest pending frame of any connected camera
//!   ([`RateMatch::Nearest`]), or the oldest frame of the slowest camera ([`RateMatch::Slowest`],
//!   measured from each camera's timestamps), so a 60 fps camera paired with a 30 fps one
//!   contributes every other frame.
//! - **Match.** Every other camera contributes its frame nearest the anchor, once that choice is
//!   settled: a frame at or after the anchor is pending, or the camera's next frame (predicted from
//!   its period) would be farther away. Older frames of that camera are dropped as
//!   [`DropReason::Superseded`]. When the anchor's own next frame is nearer to a match, the anchor
//!   gives way (`Superseded`).
//! - **Policy.** [`GroupPolicy::Strict`] needs every registered camera; [`GroupPolicy::Partial`]
//!   emits what it has at the deadline (at least [`GroupConfig::min_members`]);
//!   [`GroupPolicy::Latest`] keeps only the newest complete group of the connected cameras.
//!
//! The grouper is generic over the item it groups. With Styx frames, `Grouper<FrameLease<M>, N>`
//! takes the leases themselves through [`Grouper::push_frame`]: a group holds shares of the
//! capture buffers, and a dropped frame releases its buffer at once.
//!
//! Not here: mapping a sensor clock onto the grouper's clock, the drift estimate, and metric
//! export. Those belong to the layer that owns the cameras and the metrics.

#![no_std]

extern crate alloc;

mod config;
mod group;
mod grouper;
mod reason;
mod ring;
mod stats;

#[cfg(test)]
mod tests;

pub use config::{GroupConfig, GroupPolicy, RateMatch};
pub use group::{CameraIndex, FrameGroup, Member};
pub use grouper::{Grouper, Pushed};
pub use reason::DropReason;
pub use stats::{SPREAD_WINDOW, Stats};
