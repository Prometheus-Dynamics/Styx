//! MCAP recordings (the default format), laid out for ROS 2 tooling.
//!
//! - Profile `ros2`, messages CDR-encoded with `ros2msg` schemas.
//! - `/styx/image` (`sensor_msgs/msg/Image`) or `/styx/image/compressed`
//!   (`sensor_msgs/msg/CompressedImage`): the frame's pixels or bitstream.
//! - `/styx/pyramid/<level>` (`sensor_msgs/msg/Image`): pyramid companions.
//! - `/styx/frame_meta` (`styx/msg/FrameMeta`): one message per image message with what ROS
//!   image types cannot hold (exact format, clock, backend sequence, crop, timing, and for
//!   sensors Styx drives the exposure, gains and frame timing that produced the frame).
//!
//! Format version 2 appends the native sensor values to `styx/msg/FrameMeta` (backend 3);
//! version 1 recordings (native frames stored as V4L2 buffers, no sensor values) still read.
//! - Metadata record `styx.recording`: the device and capture mode.
//!
//! Messages of one frame share its MCAP `sequence` (the frame index) and `log_time` (the frame
//! timestamp). Raw pixels are the visible rows, tightly packed, plane after plane; `step` is
//! plane 0's row bytes. Formats without a ROS image encoding use `styx:<FOURCC>`.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::time::Duration;

use mcap::records::{MessageHeader, Metadata};
use styx_core::prelude::*;

use super::cdr::{CdrReader, CdrWriter};
use super::{RecordingHeader, ReplayError, frame_from_payload};
use crate::DeviceIdentity;

pub(super) const IMAGE_TOPIC: &str = "/styx/image";
pub(super) const COMPRESSED_TOPIC: &str = "/styx/image/compressed";
pub(super) const META_TOPIC: &str = "/styx/frame_meta";
pub(super) const RECORDING_METADATA: &str = "styx.recording";
const FORMAT_VERSION: &str = "2";
/// Versions this reader understands: 1 lacks the native sensor values.
const READABLE_VERSIONS: [&str; 2] = ["1", "2"];

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
uint8 backend          # 0 none, 1 v4l2, 2 libcamera, 3 native (a sensor Styx drives)
uint32 sequence
uint32 v4l2_bytes_used     # also native
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
uint8[] pyramid_levels       # companions of this frame, each on /styx/pyramid/<level>
# Native sensor values (backend 3; zero otherwise). Absent in format version 1.
uint64 native_exposure_ns
float32 native_analog_gain
float32 native_digital_gain
uint64 native_frame_duration_ns
uint32 native_frame_length   # lines
bool native_verified         # read back from the frame's embedded data, not predicted
bool native_error            # the receiver flagged the frame
# What the 3A loop knew of the frame (processed native captures). Absent in earlier version 2
# recordings.
uint64 native_frame_end_ns   # its last row received, CLOCK_MONOTONIC; 0: unknown
bool aaa_valid
float32[3] aaa_colour_gains  # white balance (red, green, blue) the ISP applied
float32 aaa_colour_temperature_k
float32 aaa_lux
bool aaa_ae_locked
bool aaa_awb_converged
uint64 aaa_flicker_period_ns # 0: no flicker detected
bool lens_valid
float32 lens_dioptres
uint8 lens_af_state          # 0 idle, 1 scanning, 2 focused, 3 failed
uint8 lens_settled           # 0 unknown, 1 no, 2 yes";

fn image_def() -> String {
    format!(
        "std_msgs/Header header\nuint32 height\nuint32 width\nstring encoding\nuint8 is_bigendian\nuint32 step\nuint8[] data\n{TIME_DEF}"
    )
}

fn compressed_def() -> String {
    format!("std_msgs/Header header\nstring format\nuint8[] data\n{TIME_DEF}")
}

pub(super) fn pyramid_topic(level: u8) -> String {
    format!("/styx/pyramid/{level}")
}

pub(crate) struct McapRecorder {
    pub(super) writer: mcap::Writer<BufWriter<File>>,
    image_schema: u16,
    compressed_schema: u16,
    meta_channel: u16,
    /// Image channels by topic, added when first used so recordings have no empty topics.
    channels: HashMap<String, u16>,
    /// `/styx/clock`, when clock samples are on ([`super::RecorderOptions::clock_period`]).
    pub(super) clock: Option<super::mcap_host::ClockRecord>,
}

/// Where an encoded message goes.
pub(crate) enum Target {
    Meta,
    Image { topic: String, compressed: bool },
}

/// One message of a frame, encoded ([`McapRecorder::encode_frame`]).
pub(crate) struct EncodedMsg {
    target: Target,
    log_time: u64,
    pub(super) data: Vec<u8>,
}

impl McapRecorder {
    pub(crate) fn create(path: &Path, header: &RecordingHeader) -> Result<Self, ReplayError> {
        let options = super::RecorderOptions {
            clock_period: None,
            ..Default::default()
        };
        Self::create_with(path, header, &options)
    }

    pub(crate) fn create_with(
        path: &Path,
        header: &RecordingHeader,
        options: &super::RecorderOptions,
    ) -> Result<Self, ReplayError> {
        let write_options = super::mcap_host::write_options(options)?;
        let file = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let mut writer = write_options
            .profile("ros2")
            .library(concat!("styx ", env!("CARGO_PKG_VERSION")))
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
        let clock = options
            .clock_period
            .map(|period| super::mcap_host::ClockRecord::add(&mut writer, period))
            .transpose()?;
        let mut out = Self {
            writer,
            image_schema,
            compressed_schema,
            meta_channel,
            channels: HashMap::new(),
            clock,
        };
        out.clock_if_due()?;
        Ok(out)
    }

    /// `frame`'s messages: its metadata and pixels, then each pyramid level's (other companions
    /// are not recorded). Needs nothing of the recorder, so it runs on the caller's thread.
    pub(crate) fn encode_frame(frame: &FrameLease) -> Result<Vec<EncodedMsg>, ReplayError> {
        let compressed = frame.meta().format.code.layout_info().compressed;
        let topic = if compressed {
            COMPRESSED_TOPIC
        } else {
            IMAGE_TOPIC
        };
        let pyramid: Vec<(u8, &FrameLease)> = frame
            .companions()
            .filter_map(|(kind, companion)| match kind {
                CompanionKind::Pyramid { level } => Some((level, companion)),
                CompanionKind::Scaled
                | CompanionKind::Overview
                | CompanionKind::Region { .. }
                | CompanionKind::SensorRaw => None,
            })
            .collect();
        let levels: Vec<u8> = pyramid.iter().map(|(level, _)| *level).collect();
        let mut msgs = Vec::with_capacity(2 + 2 * pyramid.len());
        encode_part(&mut msgs, frame, topic.to_string(), &levels)?;
        for (level, companion) in pyramid {
            encode_part(&mut msgs, companion, pyramid_topic(level), &[])?;
        }
        Ok(msgs)
    }

    /// Writes a frame's messages under MCAP `sequence`.
    pub(crate) fn write_encoded(
        &mut self,
        sequence: u32,
        msgs: &[EncodedMsg],
    ) -> Result<(), ReplayError> {
        for m in msgs {
            let channel_id = match &m.target {
                Target::Meta => self.meta_channel,
                Target::Image { topic, compressed } => self.channel(topic, *compressed)?,
            };
            let header = MessageHeader {
                channel_id,
                sequence,
                log_time: m.log_time,
                publish_time: m.log_time,
            };
            self.writer
                .write_to_known_channel(&header, &m.data)
                .map_err(mcap_error)?;
        }
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

    pub(crate) fn finish(mut self) -> Result<(), ReplayError> {
        self.clock_now()?;
        self.writer.finish().map_err(mcap_error)?;
        let file = self.writer.into_inner();
        file.get_ref().sync_all()?;
        Ok(())
    }
}

/// One image part's metadata and pixel messages.
fn encode_part(
    msgs: &mut Vec<EncodedMsg>,
    frame: &FrameLease,
    topic: String,
    levels: &[u8],
) -> Result<(), ReplayError> {
    let compressed = frame.meta().format.code.layout_info().compressed;
    let log_time = frame.meta().timestamp;
    msgs.push(EncodedMsg {
        target: Target::Meta,
        log_time,
        data: encode_meta(frame.meta(), &topic, levels),
    });
    let data = if compressed {
        encode_compressed(frame)?
    } else {
        encode_image(frame)?
    };
    msgs.push(EncodedMsg {
        target: Target::Image { topic, compressed },
        log_time,
        data,
    });
    Ok(())
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
        ("color".into(), format.color.code().to_string()),
        (
            "interval".into(),
            header
                .interval
                .map(|i| format!("{}/{}", i.numerator, i.denominator))
                .unwrap_or_default(),
        ),
    ])
}

pub(super) fn header_from_map(
    map: &BTreeMap<String, String>,
) -> Result<RecordingHeader, ReplayError> {
    let get = |key: &str| {
        map.get(key)
            .ok_or(ReplayError::Corrupt("recording metadata incomplete"))
    };
    let num = |key: &str| -> Result<u32, ReplayError> {
        get(key)?
            .parse()
            .map_err(|_| ReplayError::Corrupt("recording metadata not a number"))
    };
    if !READABLE_VERSIONS.contains(&get("styx_format_version")?.as_str()) {
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
            ColorSpace::from_code(num("color")? as u8),
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

pub(super) fn encode_meta(meta: &FrameMeta, topic: &str, levels: &[u8]) -> Vec<u8> {
    let mut w = CdrWriter::with_capacity(256);
    encode_meta_v1_fields(&mut w, meta, topic, levels);
    let n = meta.native().copied().unwrap_or_default();
    w.u64(n.exposure_ns);
    w.f32(n.analog_gain);
    w.f32(n.digital_gain);
    w.u64(n.frame_duration_ns);
    w.u32(n.frame_length);
    w.bool(n.verified);
    w.bool(n.error);
    super::mcap_native::encode_aaa(&mut w, &n);
    w.finish()
}

/// The fields format version 1 has (all of a version 1 message).
fn encode_meta_v1_fields(w: &mut CdrWriter, meta: &FrameMeta, topic: &str, levels: &[u8]) {
    w.string(topic);
    w.u32(meta.format.code.to_u32());
    w.string(&meta.format.code.to_string());
    w.u32(meta.format.resolution.width.get());
    w.u32(meta.format.resolution.height.get());
    w.u8(meta.format.color.code());
    w.u64(meta.timestamp);
    w.u8(TimestampClock::code(meta.clock));
    let (backend, sequence, v4l2, memory) = match meta.backend {
        None => (0, 0, None, ""),
        Some(BackendFrameMeta::V4l2(m)) => (1, m.sequence, Some(m), ""),
        Some(BackendFrameMeta::Libcamera(m)) => (2, m.sequence, None, m.buffer_memory),
        Some(BackendFrameMeta::Native(m)) => (3, m.sequence, None, ""),
        // Recorded as the V4L2 metadata `uvcvideo` would give the frame.
        Some(BackendFrameMeta::Uvc(m)) => (1, m.sequence, Some(m.as_v4l2()), ""),
    };
    let native = meta.native().copied();
    w.u8(backend);
    w.u32(sequence);
    w.u32(v4l2.map_or(native.map_or(0, |m| m.bytes_used), |m| m.bytes_used));
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
}

/// One image message's metadata, decoded.
pub(super) struct PartMeta {
    pub(super) topic: String,
    format: MediaFormat,
    timestamp: u64,
    clock: Option<TimestampClock>,
    backend: Option<BackendFrameMeta>,
    crop: Option<FrameRect>,
    timing: FrameTiming,
    pub(super) levels: Vec<u8>,
}

pub(super) fn decode_meta(data: &[u8]) -> Result<PartMeta, ReplayError> {
    let mut r = CdrReader::new(data)?;
    let topic = r.string()?;
    let code = FourCc::from(r.u32()?);
    let _name = r.string()?;
    let resolution =
        Resolution::new(r.u32()?, r.u32()?).ok_or(ReplayError::Corrupt("zero resolution"))?;
    let format = MediaFormat::new(code, resolution, ColorSpace::from_code(r.u8()?));
    let timestamp = r.u64()?;
    let clock = TimestampClock::from_code(r.u8()?).ok_or(ReplayError::Corrupt("unknown clock"))?;
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
            ..Default::default()
        })),
        3 => Some(BackendFrameMeta::Native(NativeFrameMeta {
            sequence,
            bytes_used,
            ..Default::default()
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
    let mut part = PartMeta {
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
        levels: {
            let levels = r.bytes()?.to_vec();
            let mut seen = [false; 256];
            if levels
                .iter()
                .any(|&l| std::mem::replace(&mut seen[l as usize], true))
            {
                return Err(ReplayError::Corrupt("repeated pyramid level"));
            }
            levels
        },
    };
    // Version 2 appends the native sensor values; version 1 messages end here.
    if !r.at_end() {
        let (exposure_ns, analog_gain, digital_gain) = (r.u64()?, r.f32()?, r.f32()?);
        let (frame_duration_ns, frame_length) = (r.u64()?, r.u32()?);
        let (verified, error) = (r.bool()?, r.bool()?);
        if let Some(BackendFrameMeta::Native(m)) = &mut part.backend {
            *m = NativeFrameMeta {
                exposure_ns,
                analog_gain,
                digital_gain,
                frame_duration_ns,
                frame_length,
                verified,
                error,
                ..*m
            };
        }
        // Later version 2 recordings append the 3A record.
        if !r.at_end() {
            let (frame_end_ns, aaa) = super::mcap_native::decode_aaa(&mut r)?;
            if let Some(BackendFrameMeta::Native(m)) = &mut part.backend {
                m.frame_end_ns = frame_end_ns;
                m.aaa = aaa;
            }
        }
    } else if matches!(part.backend, Some(BackendFrameMeta::Native(_))) {
        return Err(ReplayError::Corrupt(
            "native frame without its sensor values",
        ));
    }
    Ok(part)
}

/// The pixel or bitstream bytes of an image message.
pub(super) fn decode_payload(topic: &str, data: &[u8]) -> Result<Vec<u8>, ReplayError> {
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

pub(super) fn build(
    part: PartMeta,
    payload: &[u8],
    offset: u64,
) -> Result<FrameLease, ReplayError> {
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

pub(super) fn mcap_error(err: mcap::McapError) -> ReplayError {
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
        c if &c.to_u32().to_le_bytes() == b"RG16" => "bayer_rggb16".into(),
        c if matches!(&c.to_u32().to_le_bytes(), b"BYR2" | b"BG16") => "bayer_bggr16".into(),
        c if &c.to_u32().to_le_bytes() == b"GB16" => "bayer_gbrg16".into(),
        c if &c.to_u32().to_le_bytes() == b"GR16" => "bayer_grbg16".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn grey_meta() -> FrameMeta {
        let format = MediaFormat::new(
            FourCc::GREY,
            Resolution::new(64, 32).unwrap(),
            ColorSpace::Unknown,
        );
        FrameMeta::new(format, 5_000)
    }

    #[test]
    fn version_1_messages_and_headers_still_read() {
        let mut meta = grey_meta();
        let v4l2 = BackendFrameMeta::V4l2(V4l2FrameMeta {
            sequence: 7,
            bytes_used: 2048,
            field: 1,
            flags: 0x40,
            zero_copy: true,
        });
        meta.backend = Some(v4l2.clone());
        let mut w = CdrWriter::with_capacity(256);
        encode_meta_v1_fields(&mut w, &meta, IMAGE_TOPIC, &[]);
        let part = decode_meta(&w.finish()).unwrap();
        assert_eq!(part.backend, Some(v4l2));
        assert_eq!(part.timestamp, 5_000);

        let header = super::super::tests::header(meta.format);
        let mut map = header_to_map(&header);
        assert_eq!(map["styx_format_version"], "2");
        map.insert("styx_format_version".into(), "1".into());
        assert_eq!(header_from_map(&map).unwrap().format, header.format);
        map.insert("styx_format_version".into(), "3".into());
        assert!(header_from_map(&map).is_err());
    }

    #[test]
    fn native_sensor_values_round_trip() {
        let mut meta = grey_meta();
        let native = NativeFrameMeta {
            sequence: 41,
            bytes_used: 2048,
            error: true,
            exposure_ns: 33_215_000,
            analog_gain: 7.5,
            digital_gain: 1.25,
            frame_duration_ns: 33_333_333,
            frame_length: 3662,
            verified: true,
            frame_end_ns: 0,
            aaa: None,
        };
        meta.backend = Some(BackendFrameMeta::Native(native));
        let part = decode_meta(&encode_meta(&meta, IMAGE_TOPIC, &[1, 2])).unwrap();
        assert_eq!(part.backend, Some(BackendFrameMeta::Native(native)));
        assert_eq!(part.levels, [1, 2]);
        // A native frame's message cut before its sensor values is damaged, not version 1.
        let mut w = CdrWriter::with_capacity(256);
        encode_meta_v1_fields(&mut w, &meta, IMAGE_TOPIC, &[]);
        assert!(decode_meta(&w.finish()).is_err());
    }
}
