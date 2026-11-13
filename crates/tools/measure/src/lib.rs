//! `styx-measure`: the board-side reader of a camera's frame socket, and its measurements.
//!
//! A camera's frames leave the service over a `styx-socket` ring (its path is in the service's
//! `list` answer, `socket=`). [`session::Session`] attaches to that socket as a reader in this
//! process, takes each frame, decodes its metadata with `FrameMetadata::from_bytes`, can run the
//! software ISP on it ([`isp::IspRunner`]), and counts: frames, skips, gaps, the rate from the
//! capture times, capture-to-reader latency, copies, CPU and memory. Every figure comes from this
//! board's clocks and counters; a figure the board does not report is `null` in the JSON.
//!
//! The read loop allocates nothing per frame: histograms are sized once, the ISP's pool and stage
//! are made at the first frame, and a frame is handed back when its handle drops.

pub mod cli;
pub mod clock;
pub mod control;
pub mod hist;
pub mod isp;
pub mod json;
pub mod report;
pub mod session;
pub mod stats;
