//! Import of MCAP recordings in the Styx layout (ROS 2 CDR messages) into `styxrec`.
//!
//! What is read: the `styx.recording` metadata (device, backend, format, interval); channels;
//! messages on `/styx/frame_meta` (`styx/msg/FrameMeta`, the frame's format, timestamp and native
//! sensor values) and on `/styx/image` (`sensor_msgs/msg/Image`, the pixels) or
//! `/styx/image/compressed` (the bitstream). Messages of one frame share its MCAP sequence; the
//! frame's identity is the producer's frame number in the metadata (the backend sequence). Other
//! channels (the clock, pyramid levels) are skipped: this
//! format keeps the frame and nothing else.
//!
//! Uncompressed chunks are read; a compressed chunk (zstd, lz4) is refused, since this crate
//! carries no decompressor. Import is one way: the output is `styxrec`.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};

use styx_controls::FrameMetadata;
use styx_format::{ColorSpace, FourCc, Interval, MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout, packed_layouts};

use crate::cdr::Cdr;
use crate::error::RecordError;
use crate::format::RecordingHeader;
use crate::parse::Parser;
use crate::recorder::Recorder;
use crate::replay::read_full;

const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
const OP_FOOTER: u8 = 0x02;
const OP_CHANNEL: u8 = 0x04;
const OP_MESSAGE: u8 = 0x05;
const OP_CHUNK: u8 = 0x06;
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

/// One frame read from an MCAP recording: its entry and metadata, and its bytes (the span).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedFrame {
    pub entry: FrameEntry,
    pub meta: FrameMetadata,
    pub data: Vec<u8>,
}

/// What an import wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportStats {
    /// Frames written.
    pub frames: u64,
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
    Other,
}

impl Topic {
    fn of(name: &str) -> Self {
        match name {
            META_TOPIC => Self::Meta,
            IMAGE_TOPIC => Self::Image,
            COMPRESSED_TOPIC => Self::Compressed,
            _ => Self::Other,
        }
    }
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
    header: Option<RecordingHeader>,
    pending: BTreeMap<u32, Pending>,
    ready: VecDeque<ImportedFrame>,
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
            header: None,
            pending: BTreeMap::new(),
            ready: VecDeque::new(),
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

    /// The next complete frame, or `None` at the end of the data.
    pub fn next_frame(&mut self) -> Result<Option<ImportedFrame>, RecordError> {
        loop {
            if let Some(frame) = self.ready.pop_front() {
                return Ok(Some(frame));
            }
            if !self.step()? {
                return Ok(None);
            }
        }
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
            _ => self.record(opcode, &body)?,
        }
        Ok(!self.ended)
    }

    fn cut(&mut self) -> Result<bool, RecordError> {
        self.ended = true;
        self.truncated = true;
        Ok(false)
    }

    /// An uncompressed chunk: its records, read from the chunk body.
    fn chunk(&mut self, body: &[u8]) -> Result<(), RecordError> {
        let mut p = Parser::new(body);
        p.u64()?; // message start time
        p.u64()?; // message end time
        p.u64()?; // uncompressed size
        p.u32()?; // uncompressed CRC (not checked: the frame records carry their own)
        let compression = p.mcap_string()?;
        if !compression.is_empty() {
            return Err(RecordError::Unsupported(
                "compressed MCAP chunks (zstd, lz4): record uncompressed",
            ));
        }
        let records_len =
            usize::try_from(p.u64()?).map_err(|_| RecordError::TooLarge("MCAP chunk"))?;
        let records = p.take(records_len)?;
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
            OP_CHANNEL => {
                let mut p = Parser::new(body);
                let id = p.u16()?;
                p.u16()?; // schema
                let topic = p.mcap_string()?;
                if !self.channels.contains_key(&id) && self.channels.len() >= MAX_CHANNELS {
                    return Err(RecordError::TooLarge("MCAP channels"));
                }
                self.channels.insert(id, Topic::of(&topic));
            }
            OP_MESSAGE => {
                let mut p = Parser::new(body);
                let channel = p.u16()?;
                let sequence = p.u32()?;
                p.u64()?; // log time
                p.u64()?; // publish time
                let data = p.rest();
                let topic = *self
                    .channels
                    .get(&channel)
                    .ok_or(RecordError::Corrupt("message on an unknown channel"))?;
                self.message(topic, sequence, data)?;
            }
            OP_METADATA => {
                let mut p = Parser::new(body);
                let name = p.mcap_string()?;
                if name == RECORDING_METADATA {
                    self.header = Some(header_from_map(&p.mcap_map()?)?);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn message(&mut self, topic: Topic, sequence: u32, data: &[u8]) -> Result<(), RecordError> {
        match topic {
            Topic::Other => return Ok(()),
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
            self.ready
                .push_back(assemble(u64::from(meta.sequence), meta, compressed, data)?);
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
        ..FrameMetadata::default()
    };
    Ok(ImportedFrame {
        entry: FrameEntry::new(id, part.timestamp_ns, format, layouts),
        meta,
        data,
    })
}

/// Imports the MCAP recording in `source` as `styxrec`, written to `out`. The file is read
/// twice: once for its frame sizes (the output's payload) and once for the frames.
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
    while let Some(frame) = first.next_frame()? {
        largest = largest.max(frame.data.len());
    }
    let payload = (largest as u64).max(1);
    header.payload = payload;

    source.seek(SeekFrom::Start(0))?;
    let mut second = McapReader::open(&mut *source)?;
    let mut recorder = Recorder::new(out, &header)?;
    while let Some(frame) = second.next_frame()? {
        recorder.record_frame(&frame.entry, &frame.meta, &frame.data)?;
    }
    let frames = recorder.frames();
    recorder.finish()?;
    Ok(ImportStats {
        frames,
        payload,
        truncated: second.truncated(),
    })
}
