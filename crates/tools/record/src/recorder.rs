//! Writes frames as `styxrec`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use styx_controls::FrameMetadata;
use styx_frame::{FrameEntry, FrameLease};

use crate::channel::{Channel, ChannelId, ChannelSpec};
use crate::clock::ClockPair;
use crate::crc::{Crc32, crc32};
use crate::error::RecordError;
use crate::format::{
    MAGIC, MAX_CHANNELS, MAX_SAMPLE, RecordingHeader, TAG_CHANNEL, TAG_END, TAG_FRAME, TAG_SAMPLE,
    VERSION, encode_channel, encode_frame_header, encode_header, encode_sample_head,
};

/// Writes one recording: the header when made, then each frame, channel declaration and
/// sample in the order they are given.
///
/// A frame is written from its lease (the bytes its buffer holds over the frame's span) or from
/// its parts ([`Recorder::record_frame`], used by the importers). A data channel is declared
/// once ([`Recorder::declare`]) and its samples written with [`Recorder::sample`]: a timestamp on
/// `CLOCK_MONOTONIC` and the sample's bytes. Write each record as it arrives; replay puts frames
/// and samples in timestamp order. Nothing is buffered beyond the writer's own buffer and one
/// frame header, and writing a frame or a sample allocates nothing.
#[derive(Debug)]
pub struct Recorder<W: Write> {
    out: W,
    payload: u64,
    frames: u64,
    samples: u64,
    channels: Vec<String>,
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
            samples: 0,
            channels: Vec::new(),
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

    /// Declares a data channel and returns its number, with both clocks read now
    /// ([`ClockPair::now`]) kept in the declaration. A channel may be declared at any point;
    /// its samples follow its declaration. Refuses a name already declared, and more than
    /// [`crate::format::MAX_CHANNELS`] channels.
    pub fn declare(&mut self, spec: ChannelSpec) -> Result<ChannelId, RecordError> {
        self.declare_at(spec, ClockPair::now())
    }

    /// [`Recorder::declare`] with the clocks given: for an importer, whose declaration happened
    /// when the original was recorded.
    pub fn declare_at(
        &mut self,
        spec: ChannelSpec,
        declared: ClockPair,
    ) -> Result<ChannelId, RecordError> {
        if self.channels.len() >= MAX_CHANNELS {
            return Err(RecordError::Channel("too many channels"));
        }
        if self.channels.contains(&spec.name) {
            return Err(RecordError::Channel("a channel of that name is declared"));
        }
        let id = ChannelId(self.channels.len() as u16);
        let channel = Channel { id, spec, declared };
        encode_channel(&mut self.scratch, &channel)?;
        self.out.write_all(&[TAG_CHANNEL])?;
        self.out
            .write_all(&(self.scratch.len() as u32).to_le_bytes())?;
        self.out.write_all(&self.scratch)?;
        self.out.write_all(&crc32(&self.scratch).to_le_bytes())?;
        self.channels.push(channel.spec.name);
        Ok(id)
    }

    /// Writes one sample of a declared channel: its time on `CLOCK_MONOTONIC` (convert a
    /// `CLOCK_BOOTTIME` time with [`crate::ClockPair::monotonic_from_boottime`] first) and its
    /// bytes, at most [`crate::format::MAX_SAMPLE`]. Allocates nothing.
    pub fn sample(
        &mut self,
        channel: ChannelId,
        timestamp_ns: u64,
        data: &[u8],
    ) -> Result<(), RecordError> {
        if usize::from(channel.0) >= self.channels.len() {
            return Err(RecordError::Channel("sample of a channel not declared"));
        }
        if data.len() > MAX_SAMPLE {
            return Err(RecordError::TooLarge("sample"));
        }
        let head = encode_sample_head(channel, timestamp_ns, data.len() as u32);
        let crc = Crc32::new().update(&head).update(data).value();
        self.out.write_all(&[TAG_SAMPLE])?;
        self.out.write_all(&head)?;
        self.out.write_all(data)?;
        self.out.write_all(&crc.to_le_bytes())?;
        self.samples += 1;
        Ok(())
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Samples written so far, over every channel.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Writes the end marker, flushes, and returns the writer. A recording that is never
    /// finished still replays: it ends at its last whole frame.
    pub fn finish(mut self) -> Result<W, RecordError> {
        self.out.write_all(&[TAG_END])?;
        self.out.flush()?;
        Ok(self.out)
    }
}
