//! The frame types this crate hands out. Every use of the Lemnos frame stream's types goes through
//! these two names, so the metadata type changes in one place.
//!
//! Each frame carries its [`FrameMetadata`]: the controls in force on it, from the native controls
//! of its session (see [`crate::Session::request_controls`]).

use styx_controls::FrameMetadata;

/// The frame stream of one receiver, as styx-lemnos opens it, with each frame's metadata.
pub type Frames = styx_lemnos::Frames<FrameMetadata>;

/// One frame as a lease over its buffer and its metadata.
pub type Lease = styx_frame::FrameLease<FrameMetadata>;
