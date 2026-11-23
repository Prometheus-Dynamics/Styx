//! Import of MCAP recordings in the Styx layout (ROS 2 CDR messages) into `styxrec`.
//!
//! What is read: the `styx.recording` metadata (device, backend, format, interval); channels;
//! messages on `/styx/frame_meta` (`styx/msg/FrameMeta`, the frame's format, timestamp and native
//! sensor values) and on `/styx/image` (`sensor_msgs/msg/Image`, the pixels) or
//! `/styx/image/compressed` (the bitstream). Messages of one frame share its MCAP sequence; the
//! frame's identity is the producer's frame number in the metadata (the backend sequence).
//!
//! Every other topic is a data channel and becomes a `styxrec` channel ([`crate::Channel`]):
//! its topic as the name, its message encoding, its schema and its metadata; each message is a
//! sample at its log time. A channel whose metadata says `clock = CLOCK_BOOTTIME` (as a consumer's
//! recorder writes) has its times moved to `CLOCK_MONOTONIC` with the `/styx/clock` records
//! (`styx/msg/ClockSample`: monotonic, boottime, realtime and the read's spread, CDR), the latest
//! one before each message (the first in the file for messages before it). With no clock record
//! the two clocks are taken as equal, and the import says so
//! ([`ImportLosses::boottime_assumed_equal`]). A channel that carries `styx.clock` (an export
//! of `styxrec`) is already on `CLOCK_MONOTONIC`; its `styx.source_clock` is its producer's
//! clock, and both keys are dropped on import.
//!
//! What does not survive, counted in [`ImportLosses`]: attachments, metadata records other than
//! `styx.recording`, the publish time and sequence of data messages, the pyramid topics
//! (`/styx/pyramid/<level>`) and any other `/styx/` topic this reader does not know, and data
//! messages larger than [`crate::format::MAX_SAMPLE`].
//!
//! Uncompressed chunks are read, and lz4 chunks with the `mcap-lz4` feature (on by default; a
//! pure-Rust decoder). A zstd chunk, and an lz4 chunk without the feature, are refused by name.
//! A decompressed chunk must have the size the chunk states, and its CRC when the chunk gives one
//! (not 0). Import is one way: the output is `styxrec`.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};

use styx_controls::FrameMetadata;
use styx_format::{ColorSpace, FourCc, Interval, MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout, packed_layouts};

use crate::cdr::Cdr;
use crate::channel::{ChannelId, ChannelSpec, SourceClock};
use crate::clock::ClockPair;
use crate::error::RecordError;
use crate::format::{MAX_SAMPLE, RecordingHeader};
use crate::mcap_export::{BOOTTIME, CLOCK_TOPIC, STYX_CLOCK_KEY, STYX_SOURCE_CLOCK_KEY};
use crate::parse::Parser;
use crate::recorder::Recorder;
use crate::replay::read_full;

const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
const OP_FOOTER: u8 = 0x02;
const OP_SCHEMA: u8 = 0x03;
const OP_CHANNEL: u8 = 0x04;
const OP_MESSAGE: u8 = 0x05;
const OP_CHUNK: u8 = 0x06;
const OP_ATTACHMENT: u8 = 0x09;
const OP_METADATA: u8 = 0x0C;
const OP_DATA_END: u8 = 0x0F;

const RECORDING_METADATA: &str = "styx.recording";
const META_TOPIC: &str = "/styx/frame_meta";
const IMAGE_TOPIC: &str = "/styx/image";
const COMPRESSED_TOPIC: &str = "/styx/image/compressed";
/// Channels a file may declare.
const MAX_CHANNELS: usize = 4096;
/// Format versions this reader understands: 1 lacks the native sensor values.
const READABLE_VERSIONS: [&str; 2] = ["1", "2"];
/// Frames whose messages may be partly arrived at once; the oldest are dropped past this.
const MAX_PENDING: usize = 8;
/// The largest decompressed chunk read. A writer's chunk is under a megabyte, or one frame when a
/// frame is larger; 256 MiB is past any frame a camera makes.
#[cfg_attr(not(feature = "mcap-lz4"), allow(dead_code))]
const MAX_CHUNK: u64 = 256 << 20;

/// One frame read from an MCAP recording: its entry and metadata, and its bytes (the span).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedFrame {
    pub entry: FrameEntry,
    pub meta: FrameMetadata,
    pub data: Vec<u8>,
}

/// What an import could not carry into `styxrec`, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ImportLosses {
    /// Attachment records (files stored whole in the MCAP file).
    pub attachments: u64,
    /// Metadata records other than `styx.recording`.
    pub metadata_records: u64,
    /// Data messages whose MCAP sequence number was not the channel's message count (0, 1,
    /// 2, ...): a sample has none, so the number is dropped. The count is what an export writes
    /// back, so a Styx export's sequences are not lost.
    pub data_sequences: u64,
    /// Data messages whose publish time differed from their log time (the log time is kept).
    pub data_publish_times: u64,
    /// Messages on `/styx/pyramid/<level>` (pyramid companions of frames).
    pub pyramid_messages: u64,
    /// Messages on other `/styx/` topics this reader does not know.
    pub other_styx_messages: u64,
    /// Data messages larger than a sample may be.
    pub samples_too_large: u64,
    /// Data messages on a channel whose metadata says `CLOCK_BOOTTIME`, in a file with no
    /// `/styx/clock` record: their times were taken as `CLOCK_MONOTONIC` unchanged.
    pub boottime_assumed_equal: u64,
}

/// What an import wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportStats {
    /// Frames written.
    pub frames: u64,
    /// Data channels declared.
    pub channels: usize,
    /// Samples written, over every channel.
    pub samples: u64,
    /// `/styx/clock` records read (each moves later `CLOCK_BOOTTIME` times).
    pub clock_records: u64,
    /// What was not carried.
    pub losses: ImportLosses,
    /// The payload (frame buffer size) the output declares: the largest frame's span.
    pub payload: u64,
    /// The MCAP file ended inside a record (cut short while being written).
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Topic {
    Meta,
    Image,
    Compressed,
    Clock,
    Pyramid,
    /// Another `/styx/` topic.
    Other,
    /// A data channel, by its MCAP id.
    Data(u16),
}

impl Topic {
    fn of(name: &str, id: u16) -> Self {
        match name {
            META_TOPIC => Self::Meta,
            IMAGE_TOPIC => Self::Image,
            COMPRESSED_TOPIC => Self::Compressed,
            CLOCK_TOPIC => Self::Clock,
            _ if name.starts_with("/styx/pyramid/") => Self::Pyramid,
            _ if name.starts_with("/styx/") => Self::Other,
            _ => Self::Data(id),
        }
    }
}

/// A data channel read from an MCAP file, as it becomes a `styxrec` channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportedChannel {
    /// Its MCAP channel id.
    pub(crate) id: u16,
    pub(crate) spec: ChannelSpec,
    /// Its MCAP log times are on `CLOCK_BOOTTIME` and need moving.
    pub(crate) boottime: bool,
}

/// One thing read from an MCAP file, in file order.
#[derive(Debug)]
pub(crate) enum Imported {
    Frame(Box<ImportedFrame>),
    Channel(ImportedChannel),
    /// A data message: its MCAP channel, log time and bytes.
    Sample(u16, u64, Vec<u8>),
    Clock(ClockPair),
}

/// A `styx/msg/FrameMeta` message, the fields the frame needs.
#[derive(Debug, Clone, Copy, Default)]
struct FrameMetaPart {
    /// The producer's frame number (the backend sequence): the frame's identity.
    sequence: u32,
    fourcc: u32,
    width: u32,
    height: u32,
    color: u8,
    timestamp_ns: u64,
    exposure_ns: u64,
    analog_gain: f32,
    digital_gain: f32,
    lux: Option<f32>,
    colour_temperature_k: Option<u32>,
}

#[derive(Debug, Default)]
struct Pending {
    meta: Option<FrameMetaPart>,
    image: Option<(bool, Vec<u8>)>,
}

/// Streams the frames of an MCAP recording, one at a time.
#[derive(Debug)]
pub struct McapReader<R: Read + Seek> {
    src: R,
    size: u64,
    pos: u64,
    channels: BTreeMap<u16, Topic>,
    /// Messages seen per data channel (MCAP id), against which their sequences are checked.
    data_counts: BTreeMap<u16, u32>,
    /// Schemas by id: name, encoding, bytes.
    schemas: BTreeMap<u16, (String, String, Vec<u8>)>,
    header: Option<RecordingHeader>,
    pending: BTreeMap<u32, Pending>,
    ready: VecDeque<Imported>,
    losses: ImportLosses,
    ended: bool,
    truncated: bool,
}

impl<R: Read + Seek> McapReader<R> {
    /// Opens the file and reads up to its `styx.recording` metadata. Refuses a file without it.
    pub fn open(mut src: R) -> Result<Self, RecordError> {
        let size = src.seek(SeekFrom::End(0))?;
        src.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 8];
        if !read_full(&mut src, &mut magic)? || magic != MAGIC {
            return Err(RecordError::NotARecording);
        }
        let mut reader = Self {
            src,
            size,
            pos: 8,
            channels: BTreeMap::new(),
            data_counts: BTreeMap::new(),
            schemas: BTreeMap::new(),
            header: None,
            pending: BTreeMap::new(),
            ready: VecDeque::new(),
            losses: ImportLosses::default(),
            ended: false,
            truncated: false,
        };
        while reader.header.is_none() {
            if !reader.step()? {
                return Err(RecordError::Corrupt(
                    "MCAP file has no styx.recording metadata",
                ));
            }
        }
        Ok(reader)
    }

    /// The recording's header, once read (always, after [`McapReader::open`]). The payload is 0:
    /// the importer sets it from the frames.
    pub fn header(&self) -> Option<&RecordingHeader> {
        self.header.as_ref()
    }

    /// Whether the file ended inside a record.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The next complete frame, or `None` at the end of the data. Data channels and their
    /// messages are passed over.
    pub fn next_frame(&mut self) -> Result<Option<ImportedFrame>, RecordError> {
        while let Some(item) = self.next_item()? {
            if let Imported::Frame(frame) = item {
                return Ok(Some(*frame));
            }
        }
        Ok(None)
    }

    /// The next frame, channel, data message or clock record, in file order.
    pub(crate) fn next_item(&mut self) -> Result<Option<Imported>, RecordError> {
        loop {
            if let Some(item) = self.ready.pop_front() {
                return Ok(Some(item));
            }
            if !self.step()? {
                return Ok(None);
            }
        }
    }

    /// What has not been carried so far.
    pub fn losses(&self) -> ImportLosses {
        self.losses
    }

    /// Reads one top-level record (a chunk's records are read from it). `false` once the data
    /// has ended.
    fn step(&mut self) -> Result<bool, RecordError> {
        if self.ended {
            return Ok(false);
        }
        if self.pos == self.size {
            self.ended = true;
            return Ok(false);
        }
        if self.size - self.pos < 9 {
            return self.cut();
        }
        let mut head = [0u8; 9];
        self.src.read_exact(&mut head)?;
        let opcode = head[0];
        let len = u64::from_le_bytes(head[1..9].try_into().unwrap_or([0; 8]));
        if len > self.size - self.pos - 9 {
            return self.cut();
        }
        let len = usize::try_from(len).map_err(|_| RecordError::TooLarge("MCAP record"))?;
        let mut body = vec![0u8; len];
        if !read_full(&mut self.src, &mut body)? {
            return self.cut();
        }
        self.pos += 9 + len as u64;
        match opcode {
            OP_CHUNK => self.chunk(&body)?,
            OP_DATA_END | OP_FOOTER => self.ended = true,
            OP_ATTACHMENT => self.losses.attachments += 1,
            _ => self.record(opcode, &body)?,
        }
        Ok(!self.ended)
    }

    fn cut(&mut self) -> Result<bool, RecordError> {
        self.ended = true;
        self.truncated = true;
        Ok(false)
    }

    /// A chunk: its records, read from the chunk body, decompressed first when it is lz4.
    fn chunk(&mut self, body: &[u8]) -> Result<(), RecordError> {
        let mut p = Parser::new(body);
        p.u64()?; // message start time
        p.u64()?; // message end time
        let size = p.u64()?;
        let crc = p.u32()?;
        let compression = p.mcap_string()?;
        let records_len =
            usize::try_from(p.u64()?).map_err(|_| RecordError::TooLarge("MCAP chunk"))?;
        let records = p.take(records_len)?;
        match compression.as_str() {
            "" => self.records(records),
            "lz4" => self.lz4_chunk(records, size, crc),
            "zstd" => Err(RecordError::Unsupported(
                "zstd MCAP chunks: record uncompressed or with lz4",
            )),
            _ => Err(RecordError::Unsupported(
                "an MCAP chunk compression other than lz4",
            )),
        }
    }

    #[cfg(feature = "mcap-lz4")]
    fn lz4_chunk(&mut self, compressed: &[u8], size: u64, crc: u32) -> Result<(), RecordError> {
        if size > MAX_CHUNK {
            return Err(RecordError::TooLarge("MCAP chunk"));
        }
        let size = usize::try_from(size).map_err(|_| RecordError::TooLarge("MCAP chunk"))?;
        let mut records = Vec::with_capacity(size);
        lz4_flex::frame::FrameDecoder::new(compressed)
            .take(size as u64 + 1)
            .read_to_end(&mut records)
            .map_err(|_| RecordError::Corrupt("MCAP lz4 chunk does not decompress"))?;
        if records.len() != size {
            return Err(RecordError::Corrupt(
                "MCAP lz4 chunk is not the size it states",
            ));
        }
        if crc != 0 && crate::crc::crc32(&records) != crc {
            return Err(RecordError::Corrupt("MCAP lz4 chunk fails its CRC"));
        }
        self.records(&records)
    }

    #[cfg(not(feature = "mcap-lz4"))]
    fn lz4_chunk(&mut self, _: &[u8], _: u64, _: u32) -> Result<(), RecordError> {
        Err(RecordError::Unsupported(
            "lz4 MCAP chunks: this build has no lz4 (feature mcap-lz4)",
        ))
    }

    /// A chunk's records.
    fn records(&mut self, records: &[u8]) -> Result<(), RecordError> {
        let mut q = Parser::new(records);
        while !q.at_end() {
            let opcode = q.u8()?;
            let len =
                usize::try_from(q.u64()?).map_err(|_| RecordError::TooLarge("MCAP record"))?;
            let record = q.take(len)?;
            if opcode == OP_CHUNK {
                return Err(RecordError::Corrupt("MCAP chunk inside a chunk"));
            }
            self.record(opcode, record)?;
        }
        Ok(())
    }

    fn record(&mut self, opcode: u8, body: &[u8]) -> Result<(), RecordError> {
        match opcode {
            OP_SCHEMA => {
                let mut p = Parser::new(body);
                let id = p.u16()?;
                let name = p.mcap_string()?;
                let encoding = p.mcap_string()?;
                let len = p.u32()? as usize;
                if len > crate::format::MAX_SCHEMA {
                    return Err(RecordError::TooLarge("MCAP schema"));
                }
                let data = p.take(len)?.to_vec();
                if !self.schemas.contains_key(&id) && self.schemas.len() >= MAX_CHANNELS {
                    return Err(RecordError::TooLarge("MCAP schemas"));
                }
                self.schemas.insert(id, (name, encoding, data));
            }
            OP_CHANNEL => {
                let mut p = Parser::new(body);
                let id = p.u16()?;
                let schema = p.u16()?;
                let topic = p.mcap_string()?;
                let encoding = p.mcap_string()?;
                let metadata = p.mcap_map()?;
                if self.channels.contains_key(&id) {
                    // Repeated in the summary or a later chunk: already known.
                    return Ok(());
                }
                if self.channels.len() >= MAX_CHANNELS {
                    return Err(RecordError::TooLarge("MCAP channels"));
                }
                let kind = Topic::of(&topic, id);
                self.channels.insert(id, kind);
                if kind == Topic::Data(id) {
                    let channel = self.data_channel(id, schema, topic, encoding, metadata)?;
                    self.ready.push_back(Imported::Channel(channel));
                }
            }
            OP_MESSAGE => {
                let mut p = Parser::new(body);
                let channel = p.u16()?;
                let sequence = p.u32()?;
                let log_time = p.u64()?;
                let publish_time = p.u64()?;
                let data = p.rest();
                let topic = *self
                    .channels
                    .get(&channel)
                    .ok_or(RecordError::Corrupt("message on an unknown channel"))?;
                match topic {
                    Topic::Data(id) => {
                        let count = self.data_counts.entry(channel).or_insert(0);
                        if sequence != *count {
                            self.losses.data_sequences += 1;
                        }
                        *count = count.wrapping_add(1);
                        if publish_time != log_time {
                            self.losses.data_publish_times += 1;
                        }
                        if data.len() > MAX_SAMPLE {
                            self.losses.samples_too_large += 1;
                        } else {
                            self.ready
                                .push_back(Imported::Sample(id, log_time, data.to_vec()));
                        }
                    }
                    Topic::Clock => {
                        let mut c = Cdr::new(data)?;
                        let pair = ClockPair {
                            monotonic_ns: c.u64()?,
                            boottime_ns: c.u64()?,
                        };
                        self.ready.push_back(Imported::Clock(pair));
                    }
                    Topic::Pyramid => self.losses.pyramid_messages += 1,
                    Topic::Other => self.losses.other_styx_messages += 1,
                    Topic::Meta | Topic::Image | Topic::Compressed => {
                        self.message(topic, sequence, data)?;
                    }
                }
            }
            OP_METADATA => {
                let mut p = Parser::new(body);
                let name = p.mcap_string()?;
                if name == RECORDING_METADATA {
                    self.header = Some(header_from_map(&p.mcap_map()?)?);
                } else {
                    self.losses.metadata_records += 1;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A data channel's declaration from its MCAP channel and schema.
    fn data_channel(
        &self,
        id: u16,
        schema: u16,
        topic: String,
        encoding: String,
        mut metadata: BTreeMap<String, String>,
    ) -> Result<ImportedChannel, RecordError> {
        let (schema_name, schema_encoding, schema) = match schema {
            0 => Default::default(),
            n => self
                .schemas
                .get(&n)
                .cloned()
                .ok_or(RecordError::Corrupt("MCAP channel names an unknown schema"))?,
        };
        if metadata.len() > crate::format::MAX_KEYS {
            return Err(RecordError::TooLarge("MCAP channel metadata"));
        }
        let (clock, boottime) = match metadata.remove(STYX_CLOCK_KEY) {
            // An export of styxrec: already monotonic.
            Some(_) => {
                let source = metadata.remove(STYX_SOURCE_CLOCK_KEY);
                let clock = if source.as_deref() == Some(BOOTTIME) {
                    SourceClock::Boottime
                } else {
                    SourceClock::Monotonic
                };
                (clock, false)
            }
            None if metadata.get("clock").map(String::as_str) == Some(BOOTTIME) => {
                (SourceClock::Boottime, true)
            }
            None => (SourceClock::Monotonic, false),
        };
        Ok(ImportedChannel {
            id,
            spec: ChannelSpec {
                name: topic,
                encoding,
                schema_name,
                schema_encoding,
                schema,
                clock,
                metadata,
            },
            boottime,
        })
    }

    fn message(&mut self, topic: Topic, sequence: u32, data: &[u8]) -> Result<(), RecordError> {
        match topic {
            Topic::Clock | Topic::Pyramid | Topic::Other | Topic::Data(_) => return Ok(()),
            Topic::Meta => {
                let meta = frame_meta(data)?;
                self.pending_for(sequence).meta = Some(meta);
            }
            Topic::Image | Topic::Compressed => {
                let compressed = topic == Topic::Compressed;
                let bytes = image_data(data, compressed)?;
                self.pending_for(sequence).image = Some((compressed, bytes));
            }
        }
        let complete = self
            .pending
            .get(&sequence)
            .is_some_and(|p| p.meta.is_some() && p.image.is_some());
        if complete && let Some(part) = self.pending.remove(&sequence) {
            let (Some(meta), Some((compressed, data))) = (part.meta, part.image) else {
                return Err(RecordError::Corrupt("incomplete frame"));
            };
            self.ready.push_back(Imported::Frame(Box::new(assemble(
                u64::from(meta.sequence),
                meta,
                compressed,
                data,
            )?)));
        }
        Ok(())
    }

    fn pending_for(&mut self, sequence: u32) -> &mut Pending {
        if !self.pending.contains_key(&sequence) && self.pending.len() >= MAX_PENDING {
            self.pending.pop_first();
        }
        self.pending.entry(sequence).or_default()
    }
}

/// The header from the `styx.recording` metadata map (format versions 1 and 2).
fn header_from_map(map: &BTreeMap<String, String>) -> Result<RecordingHeader, RecordError> {
    let get = |key: &str| {
        map.get(key)
            .ok_or(RecordError::Corrupt("recording metadata incomplete"))
    };
    let num = |key: &str| -> Result<u32, RecordError> {
        get(key)?
            .parse()
            .map_err(|_| RecordError::Corrupt("recording metadata is not a number"))
    };
    let version = get("styx_format_version")?;
    if !READABLE_VERSIONS.contains(&version.as_str()) {
        return Err(RecordError::Corrupt(
            "unsupported Styx MCAP recording version",
        ));
    }
    let resolution = Resolution::new(num("width")?, num("height")?)
        .ok_or(RecordError::Corrupt("zero resolution"))?;
    let mut format = MediaFormat::with_defaults(FourCc::from(num("fourcc")?), resolution);
    format.color = ColorSpace::from_code(num("color")? as u8);
    let interval = match get("interval")?.split_once('/') {
        Some((n, d)) => Some(Interval {
            numerator: n
                .parse()
                .map_err(|_| RecordError::Corrupt("recording interval"))?,
            denominator: d
                .parse()
                .map_err(|_| RecordError::Corrupt("recording interval"))?,
        }),
        None => None,
    };
    Ok(RecordingHeader {
        device: get("device")?.clone(),
        keys: get("device_keys")?.lines().map(str::to_string).collect(),
        backend: get("backend")?.clone(),
        format,
        interval,
        payload: 0,
    })
}

/// The fields of a `styx/msg/FrameMeta` message the frame record keeps. Versions before 2 have
/// no native sensor values: those read as zero.
fn frame_meta(data: &[u8]) -> Result<FrameMetaPart, RecordError> {
    let mut c = Cdr::new(data)?;
    c.string()?; // image topic
    let fourcc = c.u32()?;
    c.string()?; // fourcc name
    let width = c.u32()?;
    let height = c.u32()?;
    let color = c.u8()?;
    let timestamp_ns = c.u64()?;
    c.u8()?; // clock
    c.u8()?; // backend
    let sequence = c.u32()?;
    c.u32()?; // v4l2 bytes used
    c.u32()?; // v4l2 field
    c.u32()?; // v4l2 flags
    c.bool()?; // zero copy
    c.string()?; // buffer memory
    c.bool()?; // has crop
    for _ in 0..4 {
        c.u32()?; // crop
    }
    for _ in 0..5 {
        c.i64()?; // sensor-to-capture, decode, transform, hook, encode
    }
    c.bytes()?; // pyramid levels
    let mut part = FrameMetaPart {
        sequence,
        fourcc,
        width,
        height,
        color,
        timestamp_ns,
        ..FrameMetaPart::default()
    };
    if !c.at_end() {
        part.exposure_ns = c.u64()?;
        part.analog_gain = c.f32()?;
        part.digital_gain = c.f32()?;
    }
    if !c.at_end() {
        c.u64()?; // frame duration
        c.u32()?; // frame length
        c.bool()?; // verified
        c.bool()?; // error
    }
    if !c.at_end() {
        c.u64()?; // frame end
        let valid = c.bool()?;
        for _ in 0..3 {
            c.f32()?; // colour gains
        }
        let kelvin = c.f32()?;
        let lux = c.f32()?;
        if valid {
            if kelvin.is_finite() && kelvin > 0.0 {
                part.colour_temperature_k = Some(kelvin.round() as u32);
            }
            if lux.is_finite() && lux > 0.0 {
                part.lux = Some(lux);
            }
        }
    }
    Ok(part)
}

/// The pixels (or bitstream) of an `sensor_msgs` Image or CompressedImage message.
fn image_data(data: &[u8], compressed: bool) -> Result<Vec<u8>, RecordError> {
    let mut c = Cdr::new(data)?;
    c.i32()?; // stamp seconds
    c.u32()?; // stamp nanoseconds
    c.string()?; // frame id
    if compressed {
        c.string()?; // format
    } else {
        c.u32()?; // height
        c.u32()?; // width
        c.string()?; // encoding
        if c.u8()? != 0 {
            return Err(RecordError::Unsupported("big-endian image"));
        }
        c.u32()?; // step
    }
    Ok(c.bytes()?.to_vec())
}

fn gain_milli(gain: f32) -> u32 {
    if gain.is_finite() && gain > 0.0 {
        (gain * 1000.0).round().min(u32::MAX as f32) as u32
    } else {
        0
    }
}

/// A frame from its metadata and pixels, with the layout the format implies and the bytes
/// checked against it.
fn assemble(
    id: u64,
    part: FrameMetaPart,
    compressed: bool,
    data: Vec<u8>,
) -> Result<ImportedFrame, RecordError> {
    let code = FourCc::from(part.fourcc);
    if code.is_compressed() != compressed {
        return Err(RecordError::Corrupt(
            "image message kind does not match the frame's format",
        ));
    }
    let resolution =
        Resolution::new(part.width, part.height).ok_or(RecordError::Corrupt("zero resolution"))?;
    let mut format = MediaFormat::with_defaults(code, resolution);
    format.color = ColorSpace::from_code(part.color);
    let layouts = if compressed {
        Layouts::new(&[PlaneLayout::new(0, 0, data.len())]).ok_or(RecordError::Corrupt("layout"))?
    } else {
        let layouts = packed_layouts(code, part.width, part.height, None, None)
            .map_err(|e| RecordError::Frame(format!("{e:?}")))?;
        if layouts.span() != Some(data.len()) {
            return Err(RecordError::Corrupt("image data does not match its format"));
        }
        layouts
    };
    let meta = FrameMetadata {
        sensor_timestamp_ns: part.timestamp_ns,
        exposure_us: u32::try_from(part.exposure_ns / 1000).unwrap_or(u32::MAX),
        analogue_gain_milli: gain_milli(part.analog_gain),
        digital_gain_milli: gain_milli(part.digital_gain),
        lux: part.lux,
        colour_temperature_k: part.colour_temperature_k,
        ..FrameMetadata::default()
    };
    Ok(ImportedFrame {
        entry: FrameEntry::new(id, part.timestamp_ns, format, layouts),
        meta,
        data,
    })
}

/// Imports the MCAP recording in `source` as `styxrec`, written to `out`: its frames, and its
/// data topics as channels (see the module documentation for clocks and losses). The file is
/// read twice: once for its frame sizes (the output's payload) and its first clock record, and
/// once for the records.
pub fn import_mcap<R: Read + Seek, W: Write>(
    source: &mut R,
    out: W,
) -> Result<ImportStats, RecordError> {
    source.seek(SeekFrom::Start(0))?;
    let mut first = McapReader::open(&mut *source)?;
    let mut header = first.header().cloned().ok_or(RecordError::Corrupt(
        "MCAP file has no styx.recording metadata",
    ))?;
    let mut largest = 0usize;
    let mut first_clock = None;
    while let Some(item) = first.next_item()? {
        match item {
            Imported::Frame(frame) => largest = largest.max(frame.data.len()),
            Imported::Clock(pair) if first_clock.is_none() => first_clock = Some(pair),
            _ => {}
        }
    }
    let payload = (largest as u64).max(1);
    header.payload = payload;

    source.seek(SeekFrom::Start(0))?;
    let mut second = McapReader::open(&mut *source)?;
    let mut recorder = Recorder::new(out, &header)?;
    let mut ids: BTreeMap<u16, (ChannelId, bool)> = BTreeMap::new();
    let mut clock = first_clock;
    let mut clock_records = 0u64;
    let mut assumed_equal = 0u64;
    while let Some(item) = second.next_item()? {
        match item {
            Imported::Frame(frame) => {
                recorder.record_frame(&frame.entry, &frame.meta, &frame.data)?;
            }
            Imported::Clock(pair) => {
                clock = Some(pair);
                clock_records += 1;
            }
            Imported::Channel(channel) => {
                let id = recorder.declare_at(channel.spec, clock.unwrap_or_default())?;
                ids.insert(channel.id, (id, channel.boottime));
            }
            Imported::Sample(mcap_id, log_time, data) => {
                let (id, boottime) = *ids
                    .get(&mcap_id)
                    .ok_or(RecordError::Corrupt("message on an unknown channel"))?;
                let ts = match (boottime, clock) {
                    (false, _) => log_time,
                    (true, Some(pair)) => pair.monotonic_from_boottime(log_time),
                    (true, None) => {
                        assumed_equal += 1;
                        log_time
                    }
                };
                recorder.sample(id, ts, &data)?;
            }
        }
    }
    let frames = recorder.frames();
    let samples = recorder.samples();
    recorder.finish()?;
    let mut losses = second.losses();
    losses.boottime_assumed_equal = assumed_equal;
    Ok(ImportStats {
        frames,
        channels: ids.len(),
        samples,
        clock_records,
        losses,
        payload,
        truncated: second.truncated(),
    })
}
