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
//! - Data channels ([`Channel`]): streams of timestamped samples recorded beside the frames (an
//!   IMU, a pose, a message), each declared once with its encoding and schema
//!   ([`Recorder::declare`]) and written as a `CLOCK_MONOTONIC` time and opaque bytes
//!   ([`Recorder::sample`]). [`Replay::next_item`] hands frames and samples back in timestamp
//!   order. A producer on `CLOCK_BOOTTIME` converts with [`ClockPair`] /
//!   [`monotonic_from_boottime`].
//! - [`import_mcap`] reads an MCAP recording written in the Styx layout (ROS 2 CDR messages) and
//!   writes it as `styxrec`, its other topics as data channels. [`export_mcap`] writes a
//!   `styxrec` recording back out as MCAP (lz4 chunks by default), its data channels as MCAP
//!   channels, for MCAP tools. Styx records `styxrec`.
//!
//! Headers and records are untrusted input. Every length and offset is checked before it is
//! used, each header, frame record, declaration and sample carries a CRC-32, and nothing is allocated from a length
//! until it has been checked against a limit ([`format::MAX_PAYLOAD`], [`format::MAX_HEADER_BYTES`]
//! and friends). Refusals are [`RecordError`]s, never panics.
//!
//! Frame bytes are not checksummed: the CRC covers the record's header (identity, layout and
//! metadata). A recording cut short mid-frame ends at its last whole frame and says so
//! ([`Replay::truncated`]).

#![forbid(unsafe_code)]

mod cdr;
mod channel;
mod clock;
mod crc;
mod error;
pub mod format;
mod mcap_export;
mod mcap_import;
mod mcap_write;
mod parse;
mod recorder;
mod replay;

pub use channel::{Channel, ChannelId, ChannelSpec, Sample, SourceClock};
pub use clock::{ClockPair, monotonic_from_boottime};
pub use error::RecordError;
pub use format::RecordingHeader;
pub use mcap_export::{ExportOptions, ExportStats, export_mcap};
pub use mcap_import::{ImportLosses, ImportStats, ImportedFrame, McapReader, import_mcap};
pub use mcap_write::McapCompression;
pub use recorder::Recorder;
pub use replay::{
    Item, LOOKAHEAD_FRAMES, LOOKAHEAD_SAMPLE_BYTES, LOOKAHEAD_SAMPLES, REPLAY_BUFFERS, Replay,
};
