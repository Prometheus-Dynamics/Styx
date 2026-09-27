//! MCAP recordings (the default format), laid out for ROS 2 tooling.
//!
//! - Profile `ros2`, messages CDR-encoded with `ros2msg` schemas.
//! - `/styx/image` (`sensor_msgs/msg/Image`) or `/styx/image/compressed`
//!   (`sensor_msgs/msg/CompressedImage`): the frame's pixels or bitstream.
//! - `/styx/pyramid/<level>` (`sensor_msgs/msg/Image`): pyramid companions.
//! - `/styx/frame_meta` (`styx/msg/FrameMeta`): one message per image message with what ROS
//!   image types cannot hold (exact format, clock, backend sequence, crop, timing).
//! - Metadata record `styx.recording`: the device and capture mode.
//!
//! Messages of one frame share its MCAP `sequence` (the frame index) and `log_time` (the frame
//! timestamp). Raw pixels are the visible rows, tightly packed, plane after plane; `step` is
//! plane 0's row bytes. Formats without a ROS image encoding use `styx:<FOURCC>`.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Read};
use std::path::Path;
use std::time::Duration;

use mcap::records::{MessageHeader, Metadata, Record};
use mcap::sans_io::linear_reader::{LinearReadEvent, LinearReader};
use styx_core::prelude::*;

use super::cdr::{CdrReader, CdrWriter};
use super::{RecordingHeader, ReplayError, frame_from_payload};
use crate::DeviceIdentity;

const IMAGE_TOPIC: &str = "/styx/image";
const COMPRESSED_TOPIC: &str = "/styx/image/compressed";
const META_TOPIC: &str = "/styx/frame_meta";
const RECORDING_METADATA: &str = "styx.recording";
const FORMAT_VERSION: &str = "1";

const TIME_DEF: &str =
    "================================================================================
MSG: std_msgs/Header
builtin_interfaces/Time stamp
string frame_id
================================================================================
MSG: builtin_interfaces/Time
int32 sec
uint32 nanosec";

const FRAME_META_DEF: &str = "# Styx frame metadata for one image message of a recording.
string image_topic
uint32 fourcc
string fourcc_name
uint32 width
uint32 height
uint8 color            # 0 sRGB, 1 BT.709, 2 BT.2020, 3 unknown
uint64 timestamp       # nanoseconds on `clock`
uint8 clock            # 0 unknown, 1 monotonic, 2 boottime, 3 realtime, 4 stream-relative
uint8 backend          # 0 none, 1 v4l2, 2 libcamera
uint32 sequence
uint32 v4l2_bytes_used
uint32 v4l2_field
uint32 v4l2_flags
bool v4l2_zero_copy
string buffer_memory
bool has_crop
uint32 crop_x
uint32 crop_y
uint32 crop_width
uint32 crop_height
int64 sensor_to_capture_ns   # -1: unknown
int64 decode_ns
int64 transform_ns
int64 hook_ns
int64 encode_ns
uint8[] pyramid_levels       # companions of this frame, each on /styx/pyramid/<level>";

fn image_def() -> String {
    format!(
        "std_msgs/Header header\nuint32 height\nuint32 width\nstring encoding\nuint8 is_bigendian\nuint32 step\nuint8[] data\n{TIME_DEF}"
    )
}

fn compressed_def() -> String {
    format!("std_msgs/Header header\nstring format\nuint8[] data\n{TIME_DEF}")
}

fn pyramid_topic(level: u8) -> String {
    format!("/styx/pyramid/{level}")
}

pub(crate) struct McapRecorder {
    writer: mcap::Writer<BufWriter<File>>,
    image_schema: u16,
    compressed_schema: u16,
    meta_channel: u16,
    /// Image channels by topic, added when first used so recordings have no empty topics.
    channels: HashMap<String, u16>,
    frame: u32,
}

impl McapRecorder {
    pub(crate) fn create(path: &Path, header: &RecordingHeader) -> Result<Self, ReplayError> {
        let file = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let mut writer = mcap::WriteOptions::new()
            .profile("ros2")
            .library(concat!("styx ", env!("CARGO_PKG_VERSION")))
            .compression(None)
            .create(file)
            .map_err(mcap_error)?;
        writer
            .write_metadata(&Metadata {
                name: RECORDING_METADATA.into(),
                metadata: header_to_map(header),
            })
            .map_err(mcap_error)?;
        let no_metadata = BTreeMap::new();
        let image_schema = writer
            .add_schema("sensor_msgs/msg/Image", "ros2msg", image_def().as_bytes())
            .map_err(mcap_error)?;
        let compressed_schema = writer
            .add_schema(
                "sensor_msgs/msg/CompressedImage",
                "ros2msg",
                compressed_def().as_bytes(),
            )
            .map_err(mcap_error)?;
        let meta_schema = writer
            .add_schema("styx/msg/FrameMeta", "ros2msg", FRAME_META_DEF.as_bytes())
            .map_err(mcap_error)?;
        let meta_channel = writer
            .add_channel(meta_schema, META_TOPIC, "cdr", &no_metadata)
            .map_err(mcap_error)?;
        Ok(Self {
            writer,
            image_schema,
            compressed_schema,
            meta_channel,
            channels: HashMap::new(),
            frame: 0,
        })
    }

    pub(crate) fn record(&mut self, frame: &FrameLease) -> Result<(), ReplayError> {
        let compressed = frame.meta().format.code.layout_info().compressed;
        let topic = if compressed {
            COMPRESSED_TOPIC
        } else {
            IMAGE_TOPIC
        };
        let levels: Vec<u8> = frame
            .companions()
            .map(|(CompanionKind::Pyramid { level }, _)| level)
            .collect();
        self.write_part(frame, topic, &levels)?;
        for (CompanionKind::Pyramid { level }, companion) in frame.companions() {
            self.write_part(companion, &pyramid_topic(level), &[])?;
        }
        self.frame = self.frame.wrapping_add(1);
        Ok(())
    }

    fn channel(&mut self, topic: &str, compressed: bool) -> Result<u16, ReplayError> {
        if let Some(&id) = self.channels.get(topic) {
            return Ok(id);
        }
        let schema = match compressed {
            true => self.compressed_schema,
            false => self.image_schema,
        };
        let id = self
            .writer
            .add_channel(schema, topic, "cdr", &BTreeMap::new())
            .map_err(mcap_error)?;
        self.channels.insert(topic.to_string(), id);
        Ok(id)
    }

    fn write_part(
        &mut self,
        frame: &FrameLease,
        topic: &str,
        levels: &[u8],
    ) -> Result<(), ReplayError> {
        let compressed = frame.meta().format.code.layout_info().compressed;
        let channel = self.channel(topic, compressed)?;
        let meta = frame.meta();
        let header = |channel_id| MessageHeader {
            channel_id,
            sequence: self.frame,
            log_time: meta.timestamp,
            publish_time: meta.timestamp,
        };
        let meta_msg = encode_meta(meta, topic, levels);
        self.writer
            .write_to_known_channel(&header(self.meta_channel), &meta_msg)
            .map_err(mcap_error)?;
        let image_msg = if compressed {
            encode_compressed(frame)?
        } else {
            encode_image(frame)?
        };
        self.writer
            .write_to_known_channel(&header(channel), &image_msg)
            .map_err(mcap_error)?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<(), ReplayError> {
        self.writer.finish().map_err(mcap_error)?;
        let file = self.writer.into_inner();
        file.get_ref().sync_all()?;
        Ok(())
    }
}

fn header_to_map(header: &RecordingHeader) -> BTreeMap<String, String> {
    let format = header.format;
    BTreeMap::from([
        ("styx_format_version".into(), FORMAT_VERSION.into()),
        ("device".into(), header.device.display.clone()),
        ("device_keys".into(), header.device.keys.join("\n")),
        ("backend".into(), header.backend.clone()),
        ("fourcc".into(), format.code.to_u32().to_string()),
        ("fourcc_name".into(), format.code.to_string()),
        ("width".into(), format.resolution.width.to_string()),
        ("height".into(), format.resolution.height.to_string()),
        ("color".into(), color_tag(format.color).to_string()),
        (
            "interval".into(),
            header
                .interval
                .map(|i| format!("{}/{}", i.numerator, i.denominator))
                .unwrap_or_default(),
        ),
    ])
}

fn header_from_map(map: &BTreeMap<String, String>) -> Result<RecordingHeader, ReplayError> {
    let get = |key: &str| {
        map.get(key)
            .ok_or(ReplayError::Corrupt("recording metadata incomplete"))
    };
    let num = |key: &str| -> Result<u32, ReplayError> {
        get(key)?
            .parse()
            .map_err(|_| ReplayError::Corrupt("recording metadata not a number"))
    };
    if get("styx_format_version")? != FORMAT_VERSION {
        return Err(ReplayError::Corrupt("unsupported styx recording version"));
    }
    let resolution = Resolution::new(num("width")?, num("height")?)
        .ok_or(ReplayError::Corrupt("zero resolution"))?;
    let interval = match get("interval")?.split_once('/') {
        Some((n, d)) => Some(Interval {
            numerator: n.parse().map_err(|_| ReplayError::Corrupt("interval"))?,
            denominator: d.parse().map_err(|_| ReplayError::Corrupt("interval"))?,
        }),
        None => None,
    };
    let keys = get("device_keys")?;
    Ok(RecordingHeader {
        device: DeviceIdentity {
            display: get("device")?.clone(),
            keys: keys.lines().map(str::to_string).collect(),
        },
        backend: get("backend")?.clone(),
        format: MediaFormat::new(
            FourCc::from(num("fourcc")?),
            resolution,
            color_from_tag(num("color")? as u8),
        ),
        interval,
    })
}

fn stamp(w: &mut CdrWriter, timestamp: u64) {
    w.i32((timestamp / 1_000_000_000).min(i32::MAX as u64) as i32);
    w.u32((timestamp % 1_000_000_000) as u32);
    w.string("styx");
}

fn encode_image(frame: &FrameLease) -> Result<Vec<u8>, ReplayError> {
    let meta = frame.meta();
    let len = frame
        .visible_payload_bytes()
        .map_err(|e| ReplayError::Frame(e.to_string()))?;
    let step = frame
        .visible_rows(0)
        .map_err(|e| ReplayError::Frame(e.to_string()))?
        .row_bytes();
    let mut w = CdrWriter::with_capacity(len + 128);
    stamp(&mut w, meta.timestamp);
    w.u32(meta.format.resolution.height.get());
    w.u32(meta.format.resolution.width.get());
    w.string(&ros_encoding(meta.format.code));
    w.u8(0);
    w.u32(step as u32);
    let mut copied = Ok(0);
    w.bytes_with(len, |dst| copied = frame.copy_visible_to_slice(dst));
    copied.map_err(|e| ReplayError::Frame(e.to_string()))?;
    Ok(w.finish())
}

fn encode_compressed(frame: &FrameLease) -> Result<Vec<u8>, ReplayError> {
    let meta = frame.meta();
    let planes = frame.planes();
    let data = planes.first().ok_or(ReplayError::EmptyFrame)?.data();
    let mut w = CdrWriter::with_capacity(data.len() + 64);
    stamp(&mut w, meta.timestamp);
    w.string(&ros_compressed_format(meta.format.code));
    w.bytes(data);
    Ok(w.finish())
}

fn duration_ns(d: Option<Duration>) -> i64 {
    d.map_or(-1, |d| d.as_nanos().min(i64::MAX as u128) as i64)
}

fn encode_meta(meta: &FrameMeta, topic: &str, levels: &[u8]) -> Vec<u8> {
    let mut w = CdrWriter::with_capacity(256);
    w.string(topic);
    w.u32(meta.format.code.to_u32());
    w.string(&meta.format.code.to_string());
    w.u32(meta.format.resolution.width.get());
    w.u32(meta.format.resolution.height.get());
    w.u8(color_tag(meta.format.color));
    w.u64(meta.timestamp);
    w.u8(clock_tag(meta.clock));
    let (backend, sequence, v4l2, memory) = match meta.backend {
        None => (0, 0, None, ""),
        Some(BackendFrameMeta::V4l2(m)) => (1, m.sequence, Some(m), ""),
        Some(BackendFrameMeta::Libcamera(m)) => (2, m.sequence, None, m.buffer_memory),
    };
    w.u8(backend);
    w.u32(sequence);
    w.u32(v4l2.map_or(0, |m| m.bytes_used));
    w.u32(v4l2.map_or(0, |m| m.field));
    w.u32(v4l2.map_or(0, |m| m.flags));
    w.bool(v4l2.is_some_and(|m| m.zero_copy));
    w.string(memory);
    let crop = meta.crop;
    w.bool(crop.is_some());
    let rect = crop.unwrap_or(FrameRect::new(0, 0, 0, 0));
    for v in [rect.x, rect.y, rect.width, rect.height] {
        w.u32(v);
    }
    let t = meta.timing;
    for d in [t.sensor_to_capture, t.decode, t.transform, t.hook, t.encode] {
        w.i64(duration_ns(d));
    }
    w.bytes(levels);
    w.finish()
}

/// One image message's metadata, decoded.
struct PartMeta {
    topic: String,
    format: MediaFormat,
    timestamp: u64,
    clock: Option<TimestampClock>,
    backend: Option<BackendFrameMeta>,
    crop: Option<FrameRect>,
    timing: FrameTiming,
    levels: Vec<u8>,
}

fn decode_meta(data: &[u8]) -> Result<PartMeta, ReplayError> {
    let mut r = CdrReader::new(data)?;
    let topic = r.string()?;
    let code = FourCc::from(r.u32()?);
    let _name = r.string()?;
    let resolution =
        Resolution::new(r.u32()?, r.u32()?).ok_or(ReplayError::Corrupt("zero resolution"))?;
    let format = MediaFormat::new(code, resolution, color_from_tag(r.u8()?));
    let timestamp = r.u64()?;
    let clock = clock_from_tag(r.u8()?)?;
    let backend_tag = r.u8()?;
    let sequence = r.u32()?;
    let (bytes_used, field, flags, zero_copy) = (r.u32()?, r.u32()?, r.u32()?, r.bool()?);
    let memory = r.string()?;
    let backend = match backend_tag {
        0 => None,
        1 => Some(BackendFrameMeta::V4l2(V4l2FrameMeta {
            sequence,
            bytes_used,
            field,
            flags,
            zero_copy,
        })),
        2 => Some(BackendFrameMeta::Libcamera(LibcameraFrameMeta {
            sequence,
            buffer_memory: match memory.as_str() {
                "dma-heap" => "dma-heap",
                "libcamera-allocator" => "libcamera-allocator",
                _ => "recorded",
            },
        })),
        _ => return Err(ReplayError::Corrupt("unknown backend metadata")),
    };
    let has_crop = r.bool()?;
    let rect = FrameRect::new(r.u32()?, r.u32()?, r.u32()?, r.u32()?);
    let mut durations = [None; 5];
    for d in &mut durations {
        let ns = r.i64()?;
        *d = (ns >= 0).then(|| Duration::from_nanos(ns as u64));
    }
    let [sensor_to_capture, decode, transform, hook, encode] = durations;
    Ok(PartMeta {
        topic,
        format,
        timestamp,
        clock,
        backend,
        crop: has_crop.then_some(rect),
        timing: FrameTiming {
            sensor_to_capture,
            decode,
            transform,
            hook,
            encode,
        },
        levels: r.bytes()?.to_vec(),
    })
}

/// The pixel or bitstream bytes of an image message.
fn decode_payload(topic: &str, data: &[u8]) -> Result<Vec<u8>, ReplayError> {
    let mut r = CdrReader::new(data)?;
    let (_sec, _nanosec, _frame_id) = (r.i32()?, r.u32()?, r.string()?);
    if topic == COMPRESSED_TOPIC {
        let _format = r.string()?;
    } else {
        let (_height, _width, _encoding) = (r.u32()?, r.u32()?, r.string()?);
        let (_big_endian, _step) = (r.u8()?, r.u32()?);
    }
    Ok(r.bytes()?.to_vec())
}

fn build(part: PartMeta, payload: &[u8], offset: u64) -> Result<FrameLease, ReplayError> {
    let compressed = part.format.code.layout_info().compressed;
    let mut frame = frame_from_payload(
        part.format,
        part.timestamp.saturating_add(offset),
        payload,
        compressed,
    )?;
    let meta = frame.meta_mut();
    meta.clock = part.clock;
    meta.backend = part.backend;
    meta.crop = part.crop;
    meta.timing = part.timing;
    Ok(frame)
}

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
        let main = self.main_topic().expect("complete frame").to_string();
        let part = self.metas.remove(&main).expect("main metadata");
        let levels = part.levels.clone();
        let mut frame = build(part, &self.payloads[&main], offset)?;
        for level in levels {
            let topic = pyramid_topic(level);
            let companion = build(
                self.metas.remove(&topic).expect("companion metadata"),
                &self.payloads[&topic],
                offset,
            )?;
            frame = frame
                .with_companion(CompanionKind::Pyramid { level }, companion)
                .map_err(|e| ReplayError::Frame(e.to_string()))?;
        }
        Ok(frame)
    }
}

/// Streams frames out of an MCAP recording without loading it into memory.
pub(crate) struct McapFrames<R: Read> {
    source: R,
    reader: LinearReader,
    topics: HashMap<u16, String>,
    header: Option<RecordingHeader>,
    pending: BTreeMap<u32, PendingFrame>,
    pub(crate) offset: u64,
    done: bool,
}

impl<R: Read> McapFrames<R> {
    /// Read up to the recording metadata.
    pub(crate) fn open(source: R) -> Result<(RecordingHeader, Self), ReplayError> {
        let mut frames = Self {
            source,
            reader: LinearReader::new(),
            topics: HashMap::new(),
            header: None,
            pending: BTreeMap::new(),
            offset: 0,
            done: false,
        };
        while frames.header.is_none() {
            if !frames.step()? {
                return Err(ReplayError::Corrupt(
                    "MCAP file has no styx.recording metadata",
                ));
            }
        }
        let header = frames.header.clone().expect("header");
        Ok((header, frames))
    }

    /// Process one record; `false` at the end of the file.
    fn step(&mut self) -> Result<bool, ReplayError> {
        loop {
            let event = match self.reader.next_event() {
                None => return Ok(false),
                // Cut short (e.g. the process died while recording): end at the last complete
                // frame.
                Some(Err(mcap::McapError::UnexpectedEof)) => return Ok(false),
                Some(Err(err)) => return Err(mcap_error(err)),
                Some(Ok(event)) => event,
            };
            match event {
                LinearReadEvent::ReadRequest(need) => {
                    let read = self.source.read(self.reader.insert(need))?;
                    self.reader.notify_read(read);
                }
                LinearReadEvent::Record { opcode, data } => {
                    let record = mcap::parse_record(opcode, data).map_err(mcap_error)?;
                    match record {
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
                            let pending = self.pending.entry(header.sequence).or_default();
                            if topic == META_TOPIC {
                                let part = decode_meta(&data)?;
                                pending.metas.insert(part.topic.clone(), part);
                            } else {
                                let payload = decode_payload(&topic, &data)?;
                                pending.payloads.insert(topic, payload);
                            }
                        }
                        _ => {}
                    }
                    return Ok(true);
                }
            }
        }
    }

    fn take_complete(&mut self) -> Option<PendingFrame> {
        let key = self
            .pending
            .iter()
            .find(|(_, frame)| frame.complete())
            .map(|(key, _)| *key)?;
        self.pending.remove(&key)
    }
}

impl<R: Read> Iterator for McapFrames<R> {
    type Item = Result<FrameLease, ReplayError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(frame) = self.take_complete() {
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

fn mcap_error(err: mcap::McapError) -> ReplayError {
    match err {
        mcap::McapError::Io(io) => ReplayError::Io(io),
        other => ReplayError::Mcap(other.to_string()),
    }
}

/// `sensor_msgs/Image` encoding for a raw format, or `styx:<FOURCC>` without one.
fn ros_encoding(code: FourCc) -> String {
    match code {
        FourCc::GREY | FourCc::R8 => "mono8".into(),
        FourCc::R16 => "mono16".into(),
        FourCc::RG24 | FourCc::RGB3 => "rgb8".into(),
        FourCc::BG24 | FourCc::BGR3 => "bgr8".into(),
        FourCc::RGBA => "rgba8".into(),
        FourCc::BGRA => "bgra8".into(),
        FourCc::YUYV => "yuv422_yuy2".into(),
        FourCc::UYVY => "yuv422".into(),
        FourCc::D32F => "32FC1".into(),
        FourCc::RGGB => "bayer_rggb8".into(),
        FourCc::BGGR => "bayer_bggr8".into(),
        FourCc::GBRG => "bayer_gbrg8".into(),
        FourCc::GRBG => "bayer_grbg8".into(),
        other => format!("styx:{other}"),
    }
}

fn ros_compressed_format(code: FourCc) -> String {
    match code {
        FourCc::MJPG | FourCc::JPEG => "jpeg".into(),
        FourCc::H264 => "h264".into(),
        FourCc::H265 | FourCc::HEVC => "h265".into(),
        other => format!("styx:{other}"),
    }
}

fn color_tag(color: ColorSpace) -> u8 {
    match color {
        ColorSpace::Srgb => 0,
        ColorSpace::Bt709 => 1,
        ColorSpace::Bt2020 => 2,
        ColorSpace::Unknown => 3,
    }
}

fn color_from_tag(tag: u8) -> ColorSpace {
    match tag {
        0 => ColorSpace::Srgb,
        1 => ColorSpace::Bt709,
        2 => ColorSpace::Bt2020,
        _ => ColorSpace::Unknown,
    }
}

fn clock_tag(clock: Option<TimestampClock>) -> u8 {
    match clock {
        None => 0,
        Some(TimestampClock::Monotonic) => 1,
        Some(TimestampClock::Boottime) => 2,
        Some(TimestampClock::Realtime) => 3,
        Some(TimestampClock::StreamRelative) => 4,
    }
}

fn clock_from_tag(tag: u8) -> Result<Option<TimestampClock>, ReplayError> {
    Ok(match tag {
        0 => None,
        1 => Some(TimestampClock::Monotonic),
        2 => Some(TimestampClock::Boottime),
        3 => Some(TimestampClock::Realtime),
        4 => Some(TimestampClock::StreamRelative),
        _ => return Err(ReplayError::Corrupt("unknown clock")),
    })
}
