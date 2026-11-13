//! The frame types this crate hands out. Every use of the Lemnos frame stream's types goes through
//! these two names, so the metadata type changes in one place.
//!
//! Frames carry no metadata here: a lease is the frame's buffer, its identity and its time.

/// The frame stream of one receiver, as styx-lemnos opens it.
pub type Frames = styx_lemnos::Frames<()>;

/// One frame as a lease over its buffer.
pub type Lease = styx_frame::FrameLease<()>;
