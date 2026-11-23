//! A small MCAP writer: chunked messages (uncompressed or lz4), a message index after each
//! chunk, and a summary (schemas, channels, statistics, chunk indexes, offsets) so readers can
//! seek. Schema, channel and metadata records are written outside the chunks, ahead of the
//! messages that use them.

use std::collections::BTreeMap;
use std::io::Write;

use crate::crc::crc32;
use crate::error::RecordError;

const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
const OP_HEADER: u8 = 0x01;
const OP_FOOTER: u8 = 0x02;
const OP_SCHEMA: u8 = 0x03;
const OP_CHANNEL: u8 = 0x04;
const OP_MESSAGE: u8 = 0x05;
const OP_CHUNK: u8 = 0x06;
const OP_MESSAGE_INDEX: u8 = 0x07;
const OP_CHUNK_INDEX: u8 = 0x08;
const OP_STATISTICS: u8 = 0x0B;
const OP_METADATA: u8 = 0x0C;
const OP_SUMMARY_OFFSET: u8 = 0x0E;
const OP_DATA_END: u8 = 0x0F;

/// How the export compresses its chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum McapCompression {
    /// Uncompressed chunks.
    None,
    /// lz4 frames (MCAP's `lz4`), with the `mcap-lz4` feature.
    Lz4,
}

impl McapCompression {
    fn name(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Lz4 => "lz4",
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn put_map(out: &mut Vec<u8>, map: &BTreeMap<String, String>) {
    let len: usize = map.iter().map(|(k, v)| 8 + k.len() + v.len()).sum();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    for (k, v) in map {
        put_str(out, k);
        put_str(out, v);
    }
}

fn record(out: &mut Vec<u8>, opcode: u8, body: &[u8]) {
    out.push(opcode);
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(body);
}

pub(crate) struct McapWriter<W: Write> {
    out: W,
    pos: u64,
    compression: McapCompression,
    chunk_size: usize,
    /// The open chunk's records, uncompressed.
    chunk: Vec<u8>,
    chunk_start: u64,
    chunk_end: u64,
    /// Per channel in the open chunk: (log time, offset in `chunk`).
    index: BTreeMap<u16, Vec<(u64, u64)>>,
    compressed: Vec<u8>,
    scratch: Vec<u8>,
    schemas: Vec<Vec<u8>>,
    channels: Vec<Vec<u8>>,
    chunk_indexes: Vec<Vec<u8>>,
    metadata_count: u32,
    message_count: u64,
    first_time: u64,
    last_time: u64,
    per_channel: BTreeMap<u16, u64>,
}

impl<W: Write> McapWriter<W> {
    pub(crate) fn new(
        mut out: W,
        profile: &str,
        library: &str,
        compression: McapCompression,
        chunk_size: usize,
    ) -> Result<Self, RecordError> {
        #[cfg(not(feature = "mcap-lz4"))]
        if compression == McapCompression::Lz4 {
            return Err(RecordError::Unsupported(
                "lz4 MCAP chunks: this build has no lz4 (feature mcap-lz4)",
            ));
        }
        let mut head = Vec::with_capacity(64);
        put_str(&mut head, profile);
        put_str(&mut head, library);
        let mut bytes = Vec::with_capacity(96);
        bytes.extend_from_slice(&MAGIC);
        record(&mut bytes, OP_HEADER, &head);
        out.write_all(&bytes)?;
        Ok(Self {
            out,
            pos: bytes.len() as u64,
            compression,
            chunk_size: chunk_size.max(1024),
            chunk: Vec::with_capacity(chunk_size.max(1024) + 4096),
            chunk_start: u64::MAX,
            chunk_end: 0,
            index: BTreeMap::new(),
            compressed: Vec::new(),
            scratch: Vec::with_capacity(256),
            schemas: Vec::new(),
            channels: Vec::new(),
            chunk_indexes: Vec::new(),
            metadata_count: 0,
            message_count: 0,
            first_time: u64::MAX,
            last_time: 0,
            per_channel: BTreeMap::new(),
        })
    }

    fn top(&mut self, opcode: u8, body: &[u8]) -> Result<(), RecordError> {
        self.out.write_all(&[opcode])?;
        self.out.write_all(&(body.len() as u64).to_le_bytes())?;
        self.out.write_all(body)?;
        self.pos += 9 + body.len() as u64;
        Ok(())
    }

    /// A schema with id `id` (nonzero).
    pub(crate) fn schema(
        &mut self,
        id: u16,
        name: &str,
        encoding: &str,
        data: &[u8],
    ) -> Result<(), RecordError> {
        let mut body = Vec::with_capacity(16 + name.len() + encoding.len() + data.len());
        body.extend_from_slice(&id.to_le_bytes());
        put_str(&mut body, name);
        put_str(&mut body, encoding);
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(data);
        self.top(OP_SCHEMA, &body)?;
        self.schemas.push(body);
        Ok(())
    }

    /// A channel with id `id` on schema `schema` (0: none).
    pub(crate) fn channel(
        &mut self,
        id: u16,
        schema: u16,
        topic: &str,
        encoding: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<(), RecordError> {
        let mut body = Vec::with_capacity(32 + topic.len() + encoding.len());
        body.extend_from_slice(&id.to_le_bytes());
        body.extend_from_slice(&schema.to_le_bytes());
        put_str(&mut body, topic);
        put_str(&mut body, encoding);
        put_map(&mut body, metadata);
        self.top(OP_CHANNEL, &body)?;
        self.channels.push(body);
        Ok(())
    }

    /// A metadata record.
    pub(crate) fn metadata(
        &mut self,
        name: &str,
        map: &BTreeMap<String, String>,
    ) -> Result<(), RecordError> {
        let mut body = Vec::with_capacity(64);
        put_str(&mut body, name);
        put_map(&mut body, map);
        self.top(OP_METADATA, &body)?;
        self.metadata_count += 1;
        Ok(())
    }

    /// A message, into the open chunk (written out once the chunk reaches its size).
    pub(crate) fn message(
        &mut self,
        channel: u16,
        sequence: u32,
        log_time: u64,
        data: &[u8],
    ) -> Result<(), RecordError> {
        let offset = self.chunk.len() as u64;
        self.chunk.push(OP_MESSAGE);
        self.chunk
            .extend_from_slice(&((22 + data.len()) as u64).to_le_bytes());
        self.chunk.extend_from_slice(&channel.to_le_bytes());
        self.chunk.extend_from_slice(&sequence.to_le_bytes());
        self.chunk.extend_from_slice(&log_time.to_le_bytes());
        self.chunk.extend_from_slice(&log_time.to_le_bytes()); // publish time
        self.chunk.extend_from_slice(data);
        self.index
            .entry(channel)
            .or_default()
            .push((log_time, offset));
        self.chunk_start = self.chunk_start.min(log_time);
        self.chunk_end = self.chunk_end.max(log_time);
        self.first_time = self.first_time.min(log_time);
        self.last_time = self.last_time.max(log_time);
        self.message_count += 1;
        *self.per_channel.entry(channel).or_default() += 1;
        if self.chunk.len() >= self.chunk_size {
            self.flush_chunk()?;
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> Result<(), RecordError> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let records: &[u8] = match self.compression {
            McapCompression::None => &self.chunk,
            McapCompression::Lz4 => {
                self.compress()?;
                &self.compressed
            }
        };
        let name = self.compression.name();
        let chunk_start = self.pos;
        let mut head = Vec::with_capacity(48);
        head.extend_from_slice(&self.chunk_start.to_le_bytes());
        head.extend_from_slice(&self.chunk_end.to_le_bytes());
        head.extend_from_slice(&(self.chunk.len() as u64).to_le_bytes());
        head.extend_from_slice(&crc32(&self.chunk).to_le_bytes());
        put_str(&mut head, name);
        head.extend_from_slice(&(records.len() as u64).to_le_bytes());
        let body_len = head.len() + records.len();
        self.out.write_all(&[OP_CHUNK])?;
        self.out.write_all(&(body_len as u64).to_le_bytes())?;
        self.out.write_all(&head)?;
        self.out.write_all(records)?;
        let compressed_size = records.len() as u64;
        self.pos += 9 + body_len as u64;
        let chunk_length = self.pos - chunk_start;

        // One message index per channel in the chunk.
        let index_start = self.pos;
        let mut offsets = BTreeMap::new();
        let mut index = std::mem::take(&mut self.index);
        for (channel, entries) in &mut index {
            if entries.is_empty() {
                continue;
            }
            self.scratch.clear();
            self.scratch.extend_from_slice(&channel.to_le_bytes());
            self.scratch
                .extend_from_slice(&((entries.len() * 16) as u32).to_le_bytes());
            for (time, offset) in entries.iter() {
                self.scratch.extend_from_slice(&time.to_le_bytes());
                self.scratch.extend_from_slice(&offset.to_le_bytes());
            }
            offsets.insert(*channel, self.pos);
            let body = std::mem::take(&mut self.scratch);
            self.top(OP_MESSAGE_INDEX, &body)?;
            self.scratch = body;
            entries.clear();
        }
        self.index = index;
        let index_length = self.pos - index_start;

        let mut ci = Vec::with_capacity(80 + offsets.len() * 10);
        ci.extend_from_slice(&self.chunk_start.to_le_bytes());
        ci.extend_from_slice(&self.chunk_end.to_le_bytes());
        ci.extend_from_slice(&chunk_start.to_le_bytes());
        ci.extend_from_slice(&chunk_length.to_le_bytes());
        ci.extend_from_slice(&((offsets.len() * 10) as u32).to_le_bytes());
        for (channel, offset) in &offsets {
            ci.extend_from_slice(&channel.to_le_bytes());
            ci.extend_from_slice(&offset.to_le_bytes());
        }
        ci.extend_from_slice(&index_length.to_le_bytes());
        put_str(&mut ci, name);
        ci.extend_from_slice(&compressed_size.to_le_bytes());
        ci.extend_from_slice(&(self.chunk.len() as u64).to_le_bytes());
        self.chunk_indexes.push(ci);

        self.chunk.clear();
        self.chunk_start = u64::MAX;
        self.chunk_end = 0;
        Ok(())
    }

    #[cfg(feature = "mcap-lz4")]
    fn compress(&mut self) -> Result<(), RecordError> {
        self.compressed.clear();
        let mut encoder = lz4_flex::frame::FrameEncoder::new(&mut self.compressed);
        encoder.write_all(&self.chunk)?;
        encoder
            .finish()
            .map_err(|_| RecordError::Corrupt("lz4 encoder failed"))?;
        Ok(())
    }

    #[cfg(not(feature = "mcap-lz4"))]
    fn compress(&mut self) -> Result<(), RecordError> {
        Err(RecordError::Unsupported(
            "lz4 MCAP chunks: this build has no lz4 (feature mcap-lz4)",
        ))
    }

    /// Writes the last chunk, the data end, the summary and the footer, and returns the writer.
    pub(crate) fn finish(mut self) -> Result<W, RecordError> {
        self.flush_chunk()?;
        self.top(OP_DATA_END, &0u32.to_le_bytes())?;

        let summary_start = self.pos;
        let mut groups: Vec<(u8, u64, u64)> = Vec::new();
        let mut group = |w: &mut Self, opcode: u8, bodies: &[Vec<u8>]| -> Result<(), RecordError> {
            if bodies.is_empty() {
                return Ok(());
            }
            let start = w.pos;
            for body in bodies {
                w.top(opcode, body)?;
            }
            groups.push((opcode, start, w.pos - start));
            Ok(())
        };
        let schemas = std::mem::take(&mut self.schemas);
        group(&mut self, OP_SCHEMA, &schemas)?;
        let channels = std::mem::take(&mut self.channels);
        group(&mut self, OP_CHANNEL, &channels)?;

        let mut stats = Vec::with_capacity(64 + self.per_channel.len() * 10);
        stats.extend_from_slice(&self.message_count.to_le_bytes());
        stats.extend_from_slice(&(schemas.len() as u16).to_le_bytes());
        stats.extend_from_slice(&(channels.len() as u32).to_le_bytes());
        stats.extend_from_slice(&0u32.to_le_bytes()); // attachments
        stats.extend_from_slice(&self.metadata_count.to_le_bytes());
        stats.extend_from_slice(&(self.chunk_indexes.len() as u32).to_le_bytes());
        let (first, last) = if self.message_count == 0 {
            (0, 0)
        } else {
            (self.first_time, self.last_time)
        };
        stats.extend_from_slice(&first.to_le_bytes());
        stats.extend_from_slice(&last.to_le_bytes());
        stats.extend_from_slice(&((self.per_channel.len() * 10) as u32).to_le_bytes());
        for (channel, count) in &self.per_channel {
            stats.extend_from_slice(&channel.to_le_bytes());
            stats.extend_from_slice(&count.to_le_bytes());
        }
        group(&mut self, OP_STATISTICS, &[stats])?;
        let chunk_indexes = std::mem::take(&mut self.chunk_indexes);
        group(&mut self, OP_CHUNK_INDEX, &chunk_indexes)?;

        let offset_start = self.pos;
        for (opcode, start, length) in groups {
            let mut body = Vec::with_capacity(17);
            body.push(opcode);
            body.extend_from_slice(&start.to_le_bytes());
            body.extend_from_slice(&length.to_le_bytes());
            self.top(OP_SUMMARY_OFFSET, &body)?;
        }
        let mut footer = Vec::with_capacity(20);
        footer.extend_from_slice(&summary_start.to_le_bytes());
        footer.extend_from_slice(&offset_start.to_le_bytes());
        footer.extend_from_slice(&0u32.to_le_bytes()); // no summary CRC
        self.top(OP_FOOTER, &footer)?;
        self.out.write_all(&MAGIC)?;
        self.out.flush()?;
        Ok(self.out)
    }
}
