//! Export of a `styxrec` recording to MCAP, in the Styx MCAP layout that [`crate::import_mcap`]
//! reads (and ROS 2 tooling shows), with each data channel as an MCAP channel.
//!
//! What is written:
//! - Profile `ros2`; metadata `styx.recording` (device, keys, backend, format, interval).
//! - Per frame, under one MCAP sequence and at the frame's timestamp: `/styx/frame_meta`
//!   (`styx/msg/FrameMeta`, CDR) and `/styx/image` (`sensor_msgs/msg/Image`: the visible rows,
//!   tightly packed, plane after plane) or `/styx/image/compressed` (the bitstream).
//! - `/styx/clock` (`styx/msg/ClockSample`): one message per channel declaration, with both
//!   clocks the declaration recorded (realtime 0).
//! - Each data channel as an MCAP channel: its name as the topic, its encoding as the message
//!   encoding, its schema (when it has a name or bytes), its metadata plus `styx.clock`
//!   (`CLOCK_MONOTONIC`, the clock of every log time here) and `styx.source_clock` (the
//!   producer's clock). Each sample is a message at its timestamp; log and publish time are
//!   the same, and the sequence counts the channel's samples.
//!
//! What does not survive (MCAP has no place for it): the frame's control values (exposure and
//! gains do survive), padding between rows and planes (rows are packed), a frame id above
//! `u32::MAX`, a sensor timestamp that differs from the frame timestamp, and a lux or colour
//! temperature of exactly zero.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use styx_controls::FrameMetadata;
use styx_format::FourCc;
use styx_frame::{FrameLease, packed_layouts};

use crate::cdr::CdrWriter;
use crate::channel::{Channel, SourceClock};
use crate::error::RecordError;
use crate::format::RecordingHeader;
use crate::mcap_write::{McapCompression, McapWriter};
use crate::replay::{Item, Replay};

pub(crate) const RECORDING_METADATA: &str = "styx.recording";
pub(crate) const META_TOPIC: &str = "/styx/frame_meta";
pub(crate) const IMAGE_TOPIC: &str = "/styx/image";
pub(crate) const COMPRESSED_TOPIC: &str = "/styx/image/compressed";
pub(crate) const CLOCK_TOPIC: &str = "/styx/clock";
/// Channel metadata the export adds: the clock of the log times (always monotonic here).
pub(crate) const STYX_CLOCK_KEY: &str = "styx.clock";
/// Channel metadata the export adds: the producer's clock (the channel's [`SourceClock`]).
pub(crate) const STYX_SOURCE_CLOCK_KEY: &str = "styx.source_clock";
pub(crate) const MONOTONIC: &str = "CLOCK_MONOTONIC";
pub(crate) const BOOTTIME: &str = "CLOCK_BOOTTIME";

const SCHEMA_IMAGE: u16 = 1;
const SCHEMA_COMPRESSED: u16 = 2;
const SCHEMA_META: u16 = 3;
const SCHEMA_CLOCK: u16 = 4;
const CHANNEL_META: u16 = 0;
const CHANNEL_IMAGE: u16 = 1;
const CHANNEL_COMPRESSED: u16 = 2;
const CHANNEL_CLOCK: u16 = 3;
/// MCAP channel ids of data channels: this plus the styxrec channel id.
const FIRST_DATA_CHANNEL: u16 = 16;
/// MCAP schema ids of data channels' schemas: this plus the styxrec channel id.
const FIRST_DATA_SCHEMA: u16 = 16;

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

const CLOCK_SAMPLE_DEF: &str =
    "# Clocks read together (nanoseconds); monotonic is the middle of the reads.
uint64 monotonic_ns
uint64 boottime_ns
uint64 realtime_ns
uint32 read_spread_ns   # how long the reads took";

/// How to export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportOptions {
    /// Chunk compression. lz4 by default (with the `mcap-lz4` feature).
    pub compression: McapCompression,
    /// Uncompressed bytes per chunk before it is written (a chunk holds at least one message).
    pub chunk_size: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            compression: if cfg!(feature = "mcap-lz4") {
                McapCompression::Lz4
            } else {
                McapCompression::None
            },
            chunk_size: 1 << 20,
        }
    }
}

/// What an export wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ExportStats {
    /// Frames written.
    pub frames: u64,
    /// Samples written, over every data channel.
    pub samples: u64,
    /// Data channels written.
    pub channels: usize,
    /// Samples the replay handed out late (they are written at their own time all the same).
    pub late_samples: u64,
    /// The recording was cut short; the export holds what it had.
    pub truncated: bool,
}

/// The `styx.recording` metadata for `header`.
pub(crate) fn header_to_map(header: &RecordingHeader) -> BTreeMap<String, String> {
    let format = header.format;
    BTreeMap::from([
        ("styx_format_version".into(), "2".into()),
        ("device".into(), header.device.clone()),
        ("device_keys".into(), header.keys.join("\n")),
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

fn ros_encoding(code: FourCc) -> String {
    let bytes = code.to_u32().to_le_bytes();
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
        _ if &bytes == b"RG16" => "bayer_rggb16".into(),
        _ if matches!(&bytes, b"BYR2" | b"BG16") => "bayer_bggr16".into(),
        _ if &bytes == b"GB16" => "bayer_gbrg16".into(),
        _ if &bytes == b"GR16" => "bayer_grbg16".into(),
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

fn backend_code(backend: &str) -> u8 {
    match backend {
        "v4l2" => 1,
        "libcamera" => 2,
        "native" => 3,
        _ => 0,
    }
}

fn stamp(w: &mut CdrWriter<'_>, timestamp: u64) {
    w.i32((timestamp / 1_000_000_000).min(i32::MAX as u64) as i32);
    w.u32((timestamp % 1_000_000_000) as u32);
    w.string("styx");
}

/// A `styx/msg/FrameMeta` message (format version 2, with the 3A fields) for a frame.
fn encode_meta(
    out: &mut Vec<u8>,
    lease: &FrameLease<FrameMetadata>,
    topic: &str,
    backend: u8,
    span: usize,
) {
    let entry = lease.entry();
    let meta = lease.meta();
    let format = entry.format;
    let mut w = CdrWriter::new(out);
    w.string(topic);
    w.u32(format.code.to_u32());
    w.string(&format.code.to_string());
    w.u32(format.resolution.width.get());
    w.u32(format.resolution.height.get());
    w.u8(format.color.code());
    w.u64(entry.timestamp_ns);
    w.u8(1); // CLOCK_MONOTONIC
    w.u8(backend);
    w.u32(entry.id as u32);
    w.u32(span as u32);
    w.u32(0); // field
    w.u32(0); // flags
    w.bool(false); // zero copy
    w.string("");
    w.bool(false); // crop
    for _ in 0..4 {
        w.u32(0);
    }
    for _ in 0..5 {
        w.i64(-1); // timings unknown
    }
    w.bytes(&[]); // pyramid levels
    w.u64(u64::from(meta.exposure_us) * 1000);
    w.f32(meta.analogue_gain_milli as f32 / 1000.0);
    w.f32(meta.digital_gain_milli as f32 / 1000.0);
    w.u64(0); // frame duration
    w.u32(0); // frame length
    w.bool(false); // verified
    w.bool(false); // error
    w.u64(0); // frame end
    let aaa = meta.lux.is_some() || meta.colour_temperature_k.is_some();
    w.bool(aaa);
    for _ in 0..3 {
        w.f32(0.0); // colour gains
    }
    w.f32(meta.colour_temperature_k.map_or(0.0, |k| k as f32));
    w.f32(meta.lux.unwrap_or(0.0));
    w.bool(false); // ae locked
    w.bool(false); // awb converged
    w.u64(0); // flicker
    w.bool(false); // lens
    w.f32(0.0);
    w.u8(0);
    w.u8(0);
}

/// A `sensor_msgs/msg/Image` (rows packed) or `CompressedImage` message for a frame.
fn encode_image(
    out: &mut Vec<u8>,
    lease: &FrameLease<FrameMetadata>,
    compressed: bool,
) -> Result<(), RecordError> {
    let entry = lease.entry();
    let format = entry.format;
    let read = lease
        .read()
        .ok_or_else(|| RecordError::Frame("frame memory is not readable by the CPU".into()))?;
    let bytes = read.bytes();
    let mut w = CdrWriter::new(out);
    stamp(&mut w, entry.timestamp_ns);
    if compressed {
        let plane = entry.layouts[0];
        let data = bytes
            .get(plane.offset..plane.offset + plane.len)
            .ok_or_else(|| RecordError::Frame("bitstream past its buffer".into()))?;
        w.string(&ros_compressed_format(format.code));
        w.bytes(data);
        return Ok(());
    }
    let (width, height) = (
        format.resolution.width.get(),
        format.resolution.height.get(),
    );
    let tight = packed_layouts(format.code, width, height, None, None)
        .map_err(|e| RecordError::Frame(format!("{e:?}")))?;
    if tight.len() != entry.layouts.len() {
        return Err(RecordError::Frame(
            "frame planes do not match its format".into(),
        ));
    }
    let total = tight
        .span()
        .ok_or_else(|| RecordError::Frame("layout overflows".into()))?;
    w.u32(height);
    w.u32(width);
    w.string(&ros_encoding(format.code));
    w.u8(0);
    w.u32(tight[0].stride as u32);
    let mut result = Ok(());
    w.bytes_with(total, |dst| {
        for (to, from) in tight.iter().zip(entry.layouts.iter()) {
            let row = to.stride;
            if row == 0 || from.stride < row {
                result = Err(RecordError::Frame("plane rows shorter than packed".into()));
                return;
            }
            for r in 0..to.len / row {
                let src = from.offset + r * from.stride;
                let dst_at = to.offset + r * row;
                match bytes.get(src..src + row) {
                    Some(s) => dst[dst_at..dst_at + row].copy_from_slice(s),
                    None => {
                        result = Err(RecordError::Frame("plane past its buffer".into()));
                        return;
                    }
                }
            }
        }
    });
    result
}

/// A `styx/msg/ClockSample` message.
fn encode_clock(out: &mut Vec<u8>, monotonic_ns: u64, boottime_ns: u64) {
    let mut w = CdrWriter::new(out);
    w.u64(monotonic_ns);
    w.u64(boottime_ns);
    w.u64(0); // realtime: not recorded
    w.u32(0);
}

fn clock_name(clock: SourceClock) -> &'static str {
    match clock {
        SourceClock::Boottime => BOOTTIME,
        _ => MONOTONIC,
    }
}

struct Exporter<W: Write> {
    mcap: McapWriter<W>,
    backend: u8,
    frames: u32,
    image_channel: bool,
    compressed_channel: bool,
    exported: usize,
    sequences: Vec<u32>,
    clock_sequence: u32,
    buf: Vec<u8>,
}

impl<W: Write> Exporter<W> {
    fn channel(&mut self, channel: &Channel) -> Result<(), RecordError> {
        let n = channel.id.get();
        let spec = &channel.spec;
        let schema = if spec.schema_name.is_empty() && spec.schema.is_empty() {
            0
        } else {
            let id = FIRST_DATA_SCHEMA + n;
            self.mcap
                .schema(id, &spec.schema_name, &spec.schema_encoding, &spec.schema)?;
            id
        };
        let mut metadata = spec.metadata.clone();
        metadata.insert(STYX_CLOCK_KEY.into(), MONOTONIC.into());
        metadata.insert(STYX_SOURCE_CLOCK_KEY.into(), clock_name(spec.clock).into());
        self.mcap.channel(
            FIRST_DATA_CHANNEL + n,
            schema,
            &spec.name,
            &spec.encoding,
            &metadata,
        )?;
        encode_clock(
            &mut self.buf,
            channel.declared.monotonic_ns,
            channel.declared.boottime_ns,
        );
        self.mcap.message(
            CHANNEL_CLOCK,
            self.clock_sequence,
            channel.declared.monotonic_ns,
            &self.buf,
        )?;
        self.clock_sequence += 1;
        self.sequences.push(0);
        self.exported += 1;
        Ok(())
    }

    fn frame(&mut self, lease: &FrameLease<FrameMetadata>) -> Result<(), RecordError> {
        let entry = lease.entry();
        let compressed = entry.format.code.is_compressed();
        let (topic, channel) = if compressed {
            if !self.compressed_channel {
                self.mcap.channel(
                    CHANNEL_COMPRESSED,
                    SCHEMA_COMPRESSED,
                    COMPRESSED_TOPIC,
                    "cdr",
                    &BTreeMap::new(),
                )?;
                self.compressed_channel = true;
            }
            (COMPRESSED_TOPIC, CHANNEL_COMPRESSED)
        } else {
            if !self.image_channel {
                self.mcap.channel(
                    CHANNEL_IMAGE,
                    SCHEMA_IMAGE,
                    IMAGE_TOPIC,
                    "cdr",
                    &BTreeMap::new(),
                )?;
                self.image_channel = true;
            }
            (IMAGE_TOPIC, CHANNEL_IMAGE)
        };
        let span = entry.layouts.span().unwrap_or(0);
        encode_meta(&mut self.buf, lease, topic, self.backend, span);
        self.mcap
            .message(CHANNEL_META, self.frames, entry.timestamp_ns, &self.buf)?;
        encode_image(&mut self.buf, lease, compressed)?;
        self.mcap
            .message(channel, self.frames, entry.timestamp_ns, &self.buf)?;
        self.frames = self.frames.wrapping_add(1);
        Ok(())
    }
}

/// Exports the recording `replay` reads as MCAP, written to `out`; returns what was written
/// and the writer. Frames and samples go in timestamp order ([`Replay::next_item`]); the
/// replay holds no frame past its message, so none is dropped.
pub fn export_mcap<R: Read, W: Write>(
    replay: &mut Replay<R>,
    out: W,
    options: ExportOptions,
) -> Result<(ExportStats, W), RecordError> {
    let library = concat!("styx-record ", env!("CARGO_PKG_VERSION"));
    let mut mcap = McapWriter::new(
        out,
        "ros2",
        library,
        options.compression,
        options.chunk_size,
    )?;
    mcap.metadata(RECORDING_METADATA, &header_to_map(replay.header()))?;
    let image_def = format!(
        "std_msgs/Header header\nuint32 height\nuint32 width\nstring encoding\nuint8 is_bigendian\nuint32 step\nuint8[] data\n{TIME_DEF}"
    );
    let compressed_def = format!("std_msgs/Header header\nstring format\nuint8[] data\n{TIME_DEF}");
    mcap.schema(
        SCHEMA_IMAGE,
        "sensor_msgs/msg/Image",
        "ros2msg",
        image_def.as_bytes(),
    )?;
    mcap.schema(
        SCHEMA_COMPRESSED,
        "sensor_msgs/msg/CompressedImage",
        "ros2msg",
        compressed_def.as_bytes(),
    )?;
    mcap.schema(
        SCHEMA_META,
        "styx/msg/FrameMeta",
        "ros2msg",
        FRAME_META_DEF.as_bytes(),
    )?;
    mcap.schema(
        SCHEMA_CLOCK,
        "styx/msg/ClockSample",
        "ros2msg",
        CLOCK_SAMPLE_DEF.as_bytes(),
    )?;
    mcap.channel(
        CHANNEL_META,
        SCHEMA_META,
        META_TOPIC,
        "cdr",
        &BTreeMap::new(),
    )?;
    mcap.channel(
        CHANNEL_CLOCK,
        SCHEMA_CLOCK,
        CLOCK_TOPIC,
        "cdr",
        &BTreeMap::new(),
    )?;
    let mut ex = Exporter {
        mcap,
        backend: backend_code(&replay.header().backend),
        frames: 0,
        image_channel: false,
        compressed_channel: false,
        exported: 0,
        sequences: Vec::new(),
        clock_sequence: 0,
        buf: Vec::with_capacity(4096),
    };
    let mut stats = ExportStats::default();
    // The declarations at the start (read when the replay opened) go first.
    while ex.exported < replay.channels().len() {
        let channel = replay.channels()[ex.exported].clone();
        ex.channel(&channel)?;
    }
    // A sample's bytes, copied out of the replay so its channel can be declared first when the
    // declaration came in the same read.
    let mut sample: Vec<u8> = Vec::with_capacity(1024);
    loop {
        let mut pending_sample = None;
        match replay.next_item()? {
            None => break,
            Some(Item::Frame(lease)) => {
                ex.frame(&lease)?;
                stats.frames += 1;
            }
            Some(Item::Sample(s)) => {
                sample.clear();
                sample.extend_from_slice(s.data);
                pending_sample = Some((s.channel, s.timestamp_ns));
            }
        }
        while ex.exported < replay.channels().len() {
            let channel = replay.channels()[ex.exported].clone();
            ex.channel(&channel)?;
        }
        if let Some((channel, timestamp_ns)) = pending_sample {
            let n = usize::from(channel.get());
            let sequence = ex.sequences[n];
            ex.sequences[n] = sequence.wrapping_add(1);
            ex.mcap.message(
                FIRST_DATA_CHANNEL + channel.get(),
                sequence,
                timestamp_ns,
                &sample,
            )?;
            stats.samples += 1;
        }
    }
    // Declarations after the last item, with no samples.
    while ex.exported < replay.channels().len() {
        let channel = replay.channels()[ex.exported].clone();
        ex.channel(&channel)?;
    }
    stats.channels = ex.exported;
    stats.late_samples = replay.late_samples();
    stats.truncated = replay.truncated();
    let out = ex.mcap.finish()?;
    Ok((stats, out))
}
