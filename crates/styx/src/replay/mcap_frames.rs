//! Reading MCAP recordings: records to frames.
//!
//! Records are read here, one at a time, with every length checked against what the file
//! actually holds; the mcap crate only parses the few records Styx uses. (mcap 0.25's reader
//! buffers whatever a damaged chunk header asks for, and a damaged record length inside a chunk
//! underflows a counter, which panics in debug builds.) Styx writes uncompressed chunks, so
//! their records are streamed like top-level ones, and a recording cut short mid-chunk still
//! yields every frame written before the cut.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read};

use mcap::records::{Record, op};
use styx_core::prelude::*;

use super::mcap_format::{
    COMPRESSED_TOPIC, IMAGE_TOPIC, META_TOPIC, PartMeta, RECORDING_METADATA, build, decode_meta,
    decode_payload, header_from_map, mcap_error, pyramid_topic,
};
use super::{RecordingHeader, ReplayError};

/// Frames whose messages are still arriving. The recorder writes a frame's messages together,
/// so only a damaged file leaves more incomplete; the oldest are then dropped.
const MAX_PENDING_FRAMES: usize = 8;

#[derive(Default)]
struct PendingFrame {
    metas: HashMap<String, PartMeta>,
    payloads: HashMap<String, Vec<u8>>,
}

impl PendingFrame {
    fn main_topic(&self) -> Option<&str> {
        [IMAGE_TOPIC, COMPRESSED_TOPIC]
            .into_iter()
            .find(|t| self.metas.contains_key(*t))
    }

    fn complete(&self) -> bool {
        let Some(main) = self.main_topic() else {
            return false;
        };
        self.payloads.contains_key(main)
            && self.metas[main].levels.iter().all(|level| {
                let topic = pyramid_topic(*level);
                self.metas.contains_key(&topic) && self.payloads.contains_key(&topic)
            })
    }

    fn assemble(mut self, offset: u64) -> Result<FrameLease, ReplayError> {
        let incomplete = || ReplayError::Corrupt("incomplete frame");
        let main = self.main_topic().ok_or_else(incomplete)?.to_string();
        let part = self.metas.remove(&main).ok_or_else(incomplete)?;
        let levels = part.levels.clone();
        let mut frame = build(
            part,
            self.payloads.get(&main).ok_or_else(incomplete)?,
            offset,
        )?;
        for level in levels {
            let topic = pyramid_topic(level);
            let companion = build(
                self.metas.remove(&topic).ok_or_else(incomplete)?,
                self.payloads.get(&topic).ok_or_else(incomplete)?,
                offset,
            )?;
            frame = frame
                .with_companion(CompanionKind::Pyramid { level }, companion)
                .map_err(|e| ReplayError::Frame(e.to_string()))?;
        }
        Ok(frame)
    }
}

/// Turns records into complete frames.
#[derive(Default)]
struct Assembler {
    topics: HashMap<u16, String>,
    header: Option<RecordingHeader>,
    pending: BTreeMap<u32, PendingFrame>,
    ready: VecDeque<PendingFrame>,
}

impl Assembler {
    fn record(&mut self, opcode: u8, data: &[u8]) -> Result<(), ReplayError> {
        match opcode {
            // Only the records Styx reads are parsed, after checking their length prefixes:
            // the parser reserves what a prefix claims (up to 64 MiB) before noticing a
            // damaged one.
            op::CHANNEL | op::MESSAGE | op::METADATA => check_lengths(opcode, data)?,
            _ => return Ok(()),
        }
        match mcap::parse_record(opcode, data).map_err(mcap_error)? {
            Record::Metadata(m) if m.name == RECORDING_METADATA => {
                self.header = Some(header_from_map(&m.metadata)?);
            }
            Record::Channel(c) => {
                self.topics.insert(c.id, c.topic);
            }
            Record::Message { header, data } => {
                let topic = self
                    .topics
                    .get(&header.channel_id)
                    .ok_or(ReplayError::Corrupt("message on unknown channel"))?
                    .clone();
                let sequence = header.sequence;
                if !self.pending.contains_key(&sequence) && self.pending.len() >= MAX_PENDING_FRAMES
                {
                    self.pending.pop_first();
                }
                let pending = self.pending.entry(sequence).or_default();
                if topic == META_TOPIC {
                    let part = decode_meta(&data)?;
                    pending.metas.insert(part.topic.clone(), part);
                } else {
                    let payload = decode_payload(&topic, &data)?;
                    pending.payloads.insert(topic, payload);
                }
                if pending.complete()
                    && let Some(frame) = self.pending.remove(&sequence)
                {
                    self.ready.push_back(frame);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Streams frames out of an MCAP recording without loading it into memory (one record at a
/// time).
pub(crate) struct McapFrames<R: Read> {
    source: R,
    assembler: Assembler,
    /// No valid record is longer than this: the file size.
    record_limit: usize,
    /// The current record, reused.
    record: Vec<u8>,
    /// Record bytes left in the chunk being read, and padding after them.
    chunk_left: u64,
    chunk_padding: u64,
    pub(crate) offset: u64,
    done: bool,
}

/// Chunk header up to its records: start and end time, uncompressed size (8 bytes each), CRC
/// (4), compression name (4-byte length, empty for Styx's uncompressed chunks), records size (8).
const CHUNK_HEADER_LEN: u64 = 8 + 8 + 8 + 4 + 4 + 8;

impl<R: Read> McapFrames<R> {
    /// Read up to the recording metadata. Records longer than `record_limit` bytes (e.g. the
    /// file size) are rejected as corrupt instead of being buffered.
    pub(crate) fn open(
        mut source: R,
        record_limit: usize,
    ) -> Result<(RecordingHeader, Self), ReplayError> {
        let mut magic = [0u8; 8];
        if !read_full(&mut source, &mut magic)? || magic[..] != mcap::MAGIC[..] {
            return Err(ReplayError::NotARecording);
        }
        let mut frames = Self {
            source,
            assembler: Assembler::default(),
            record_limit,
            record: Vec::new(),
            chunk_left: 0,
            chunk_padding: 0,
            offset: 0,
            done: false,
        };
        while frames.assembler.header.is_none() {
            if !frames.step()? {
                return Err(ReplayError::Corrupt(
                    "MCAP file has no styx.recording metadata",
                ));
            }
        }
        let header = frames.assembler.header.clone().expect("header");
        Ok((header, frames))
    }

    /// Process one record; `false` at the end of the data (or where the file was cut short).
    fn step(&mut self) -> Result<bool, ReplayError> {
        let corrupt = |what| Err(ReplayError::Corrupt(what));
        if self.chunk_left == 0 && self.chunk_padding > 0 {
            let padding = std::mem::take(&mut self.chunk_padding);
            if io::copy(&mut (&mut self.source).take(padding), &mut io::sink())? < padding {
                return Ok(false);
            }
        }
        let mut head = [0u8; 9];
        if !read_full(&mut self.source, &mut head)? {
            return Ok(false);
        }
        let opcode = head[0];
        let len = u64::from_le_bytes(head[1..].try_into().expect("8 bytes"));
        let in_chunk = self.chunk_left > 0;
        if in_chunk {
            let Some(left) = self
                .chunk_left
                .checked_sub(9)
                .and_then(|l| l.checked_sub(len))
            else {
                return corrupt("MCAP record longer than its chunk");
            };
            self.chunk_left = left;
        }
        match opcode {
            op::CHUNK if in_chunk => return corrupt("MCAP chunk inside a chunk"),
            op::CHUNK => {
                let mut header = [0u8; CHUNK_HEADER_LEN as usize];
                if len < CHUNK_HEADER_LEN || !read_full(&mut self.source, &mut header)? {
                    return Ok(false);
                }
                if header[28..32] != [0; 4] {
                    return corrupt("compressed MCAP chunks are not supported");
                }
                let records = u64::from_le_bytes(header[32..40].try_into().expect("8 bytes"));
                let Some(padding) = (len - CHUNK_HEADER_LEN).checked_sub(records) else {
                    return corrupt("MCAP chunk shorter than its records");
                };
                (self.chunk_left, self.chunk_padding) = (records, padding);
                return Ok(true);
            }
            // The data section ends here; the summary after it repeats what was read.
            op::DATA_END | op::FOOTER if !in_chunk => return Ok(false),
            _ => {}
        }
        let len = match usize::try_from(len) {
            Ok(len) if len <= self.record_limit => len,
            _ => return corrupt("MCAP record longer than the file"),
        };
        // Grow with the bytes actually read, so a damaged length cannot allocate far beyond the
        // file.
        self.record.clear();
        if (&mut self.source)
            .take(len as u64)
            .read_to_end(&mut self.record)?
            < len
        {
            return Ok(false);
        }
        self.assembler.record(opcode, &self.record)?;
        Ok(true)
    }
}

/// Fill `buf`; `false` at the end of the input (including a partial read at a cut).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    match r.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

impl<R: Read> Iterator for McapFrames<R> {
    type Item = Result<FrameLease, ReplayError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(frame) = self.assembler.ready.pop_front() {
                return Some(frame.assemble(self.offset));
            }
            if self.done {
                return None;
            }
            match self.step() {
                Ok(true) => {}
                Ok(false) => self.done = true,
                Err(err) => {
                    self.done = true;
                    return Some(Err(err));
                }
            }
        }
    }
}

/// Check that the string and map length prefixes of a Channel or Metadata record fit in it.
fn check_lengths(opcode: u8, data: &[u8]) -> Result<(), ReplayError> {
    let corrupt = || ReplayError::Corrupt("MCAP length prefix past the end of its record");
    let u32_at = |at: usize| -> Result<usize, ReplayError> {
        let bytes = data.get(at..at + 4).ok_or_else(corrupt)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")) as usize)
    };
    // A length-prefixed field at `at`: the offset after it.
    let field = |at: usize, end: usize| -> Result<usize, ReplayError> {
        let next = (at + 4).checked_add(u32_at(at)?).ok_or_else(corrupt)?;
        if next > end { Err(corrupt()) } else { Ok(next) }
    };
    let string_map = |at: usize| -> Result<usize, ReplayError> {
        let end = field(at, data.len())?;
        let mut at = at + 4;
        while at < end {
            at = field(field(at, end)?, end)?;
        }
        Ok(end)
    };
    match opcode {
        // id, schema_id, topic, message_encoding, metadata
        op::CHANNEL => string_map(field(field(4, data.len())?, data.len())?).map(drop),
        // name, metadata
        op::METADATA => string_map(field(0, data.len())?).map(drop),
        // fixed header, then the payload slice
        _ => Ok(()),
    }
}
