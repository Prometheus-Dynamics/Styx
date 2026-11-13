//! Recordings: one format for frames and their metadata, replay of it as a camera, and import of
//! MCAP recordings.
//!
//! - [`Recorder`] writes the `styxrec` format: a checksummed header (the device, the backend, the
//!   format and the frame buffer size), then one record per frame: its identity, timestamp,
//!   format, plane layout and [`styx_controls::FrameMetadata`], checksummed, then the frame's
//!   bytes as the producer wrote them. See [`format`] for the byte layout.
//! - [`Replay`] reads a `styxrec` recording and hands out [`styx_frame::FrameLease`]s from its own
//!   [`styx_frame::FramePool`], with the identities, timestamps, layouts and metadata as recorded.
//!   It has the shape of a camera: a pool, and `next_frame` for the next frame. A frame the consumer has
//!   no buffer for is dropped and counted, as a camera drops.
//! - [`import_mcap`] reads an MCAP recording written in the Styx layout (ROS 2 CDR messages) and
//!   writes it as `styxrec`. It is import only: Styx writes `styxrec` from now on.
//!
//! Headers and frame records are untrusted input. Every length and offset is checked before it is
//! used, each header and frame record carries a CRC-32, and nothing is allocated from a length
//! until it has been checked against a limit ([`format::MAX_PAYLOAD`], [`format::MAX_HEADER_BYTES`]
//! and friends). Refusals are [`RecordError`]s, never panics.
//!
//! Frame bytes are not checksummed: the CRC covers the record's header (identity, layout and
//! metadata). A recording cut short mid-frame ends at its last whole frame and says so
//! ([`Replay::truncated`]).

#![forbid(unsafe_code)]

mod cdr;
mod crc;
mod error;
pub mod format;
mod mcap_import;
mod parse;
mod recorder;
mod replay;

pub use error::RecordError;
pub use format::RecordingHeader;
pub use mcap_import::{ImportStats, ImportedFrame, McapReader, import_mcap};
pub use recorder::Recorder;
pub use replay::{REPLAY_BUFFERS, Replay};
