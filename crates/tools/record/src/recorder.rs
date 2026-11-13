//! Writes frames as `styxrec`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use styx_controls::FrameMetadata;
use styx_frame::{FrameEntry, FrameLease};

use crate::crc::crc32;
use crate::error::RecordError;
use crate::format::{
    MAGIC, RecordingHeader, TAG_END, TAG_FRAME, VERSION, encode_frame_header, encode_header,
};

/// Writes one recording: the header when made, then each frame in turn.
///
/// A frame is written from its lease (the bytes its buffer holds over the frame's span) or from
/// its parts ([`Recorder::record_frame`], used by the importers). Nothing is buffered beyond the
/// writer's own buffer and one frame header.
#[derive(Debug)]
pub struct Recorder<W: Write> {
    out: W,
    payload: u64,
    frames: u64,
    scratch: Vec<u8>,
}

impl Recorder<BufWriter<File>> {
    /// Creates `path` (an existing file is truncated) and writes `header` to it.
    pub fn create(path: impl AsRef<Path>, header: &RecordingHeader) -> Result<Self, RecordError> {
        let file = File::create(path)?;
        Self::new(BufWriter::with_capacity(1 << 20, file), header)
    }
}

impl<W: Write> Recorder<W> {
    /// Writes `header` to `out`. Refuses a header that [`crate::format`] would not read back.
    pub fn new(mut out: W, header: &RecordingHeader) -> Result<Self, RecordError> {
        let body = encode_header(header)?;
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&(body.len() as u32).to_le_bytes())?;
        out.write_all(&body)?;
        out.write_all(&crc32(&body).to_le_bytes())?;
        Ok(Self {
            out,
            payload: header.payload,
            frames: 0,
            scratch: Vec::with_capacity(256),
        })
    }

    /// Records a frame from its lease: its identity, timestamp, format, layout and metadata, and
    /// the bytes of its buffer over the frame's span.
    pub fn record(&mut self, lease: &FrameLease<FrameMetadata>) -> Result<(), RecordError> {
        let span = lease
            .entry()
            .layouts
            .span()
            .ok_or_else(|| RecordError::Frame("plane layout overflows".into()))?;
        let read = lease
            .read()
            .ok_or_else(|| RecordError::Frame("frame memory is not readable by the CPU".into()))?;
        let data = read
            .bytes()
            .get(..span)
            .ok_or_else(|| RecordError::Frame("frame extends past its buffer".into()))?;
        self.record_frame(lease.entry(), lease.meta(), data)
    }

    /// Records a frame from its parts. `data` is the frame's span: exactly
    /// `entry.layouts.span()` bytes, from the start of the buffer.
    pub fn record_frame(
        &mut self,
        entry: &FrameEntry,
        meta: &FrameMetadata,
        data: &[u8],
    ) -> Result<(), RecordError> {
        let span = entry
            .layouts
            .span()
            .ok_or_else(|| RecordError::Frame("plane layout overflows".into()))?;
        if data.len() != span {
            return Err(RecordError::Frame(format!(
                "frame data is {} bytes, its layout spans {span}",
                data.len()
            )));
        }
        if span as u64 > self.payload {
            return Err(RecordError::TooLarge(
                "frame larger than the recording's payload",
            ));
        }
        encode_frame_header(&mut self.scratch, entry, meta)?;
        let frame_len = self.scratch.len() as u32;
        self.out.write_all(&[TAG_FRAME])?;
        self.out.write_all(&frame_len.to_le_bytes())?;
        self.out.write_all(&self.scratch)?;
        self.out.write_all(&crc32(&self.scratch).to_le_bytes())?;
        self.out.write_all(data)?;
        self.frames += 1;
        Ok(())
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Writes the end marker, flushes, and returns the writer. A recording that is never
    /// finished still replays: it ends at its last whole frame.
    pub fn finish(mut self) -> Result<W, RecordError> {
        self.out.write_all(&[TAG_END])?;
        self.out.flush()?;
        Ok(self.out)
    }
}
