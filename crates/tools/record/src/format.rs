//! The `styxrec` byte layout. All integers are little-endian.
//!
//! ```text
//! file    = MAGIC version:u16 header_len:u32 header[header_len] crc:u32 record* end
//! MAGIC   = "STYXRCN1"
//! header  = device:str keys:u16 key:str* backend:str format interval payload:u64
//! str     = len:u16 utf8                       (at most MAX_STRING bytes, at most MAX_KEYS keys)
//! format  = fourcc:u32 width:u32 height:u32 color:u8 range:u8
//! interval = 0x00 | 0x01 numerator:u32 denominator:u32   (both nonzero)
//! record  = 0x01 frame_len:u32 frame[frame_len] crc:u32 data[span]
//!         | 0x02 channel_len:u32 channel[channel_len] crc:u32        (version 2)
//!         | 0x03 sample                                              (version 2)
//! end     = 0x00                               (or the file simply ends)
//! frame   = id:u64 timestamp_ns:u64 format planes:u8 plane[planes] metadata
//! plane   = offset:u64 stride:u64 len:u64       (1 to 3 planes; each ends within payload)
//! span    = the end of the furthest plane; data is exactly span bytes, from offset 0
//! metadata = control[8] sensor_timestamp_ns:u64 exposure_us:u32 analogue_gain_milli:u32
//!            digital_gain_milli:u32 opt(lux:f32) opt(colour_temperature_k:u32)
//! control = 0x00 (auto) | 0x01 u32 (manual) | 0x02 u8 (flag) | 0x03 (none | 0x01 x y w h: u32)
//! opt(T)  = 0x00 | 0x01 T
//! channel = id:u16 name:str encoding:str schema_name:str schema_encoding:str
//!           schema_len:u32 schema[schema_len] clock:u8 monotonic_ns:u64 boottime_ns:u64
//! sample  = channel:u16 timestamp_ns:u64 len:u32 data[len] crc:u32   (crc over channel..data)
//! ```
//!
//! Version 2 adds the channel and sample records; a version 1 file is a version 2 file without
//! them, and this build reads both. The recorder writes version 2. A build that reads version 1
//! only refuses a version 2 file as `UnsupportedVersion(2)`.
//!
//! A channel record declares a data channel ([`crate::Channel`]): ids are numbered from zero in
//! the order declared, names are unique, `clock` is the producer's clock (0 monotonic, 1
//! boottime) and the two times are both clocks when it was declared. A sample record belongs to a
//! channel declared before it. Every timestamp in the file, frames and samples, is
//! `CLOCK_MONOTONIC` nanoseconds. Records are in the order they were written, which is the
//! order they reached the recorder, not necessarily timestamp order; replay sorts them by time
//! within one frame of lookahead ([`crate::Replay::next_item`]).
//!
//! The header and each frame record carry a CRC-32 of their bytes, so a damaged record is refused
//! rather than replayed. A channel record's CRC covers the record, and a sample's covers its
//! head and its bytes. The frame's bytes are the span of its buffer, as the producer wrote them
//! (padding between rows and planes included).
//!
//! `payload` is the size of one frame buffer: the most any frame's planes span, which the
//! recorder checks each frame against. Replay gives each of its buffers that size.

use std::io::Read;
use std::num::NonZeroU32;

use styx_controls::{Control, ControlValue, FrameMetadata, Rect};
use styx_format::{ColorRange, ColorSpace, FourCc, Interval, MAX_PLANES, MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout};

use crate::channel::{Channel, ChannelId, ChannelSpec, SourceClock};
use crate::clock::ClockPair;
use crate::crc::crc32;
use crate::error::RecordError;
use crate::parse::Parser;

/// The file's first eight bytes.
pub const MAGIC: &[u8; 8] = b"STYXRCN1";
/// The version this build writes.
pub const VERSION: u16 = 2;
/// The oldest version this build reads (version 1: frames only).
pub const MIN_VERSION: u16 = 1;
/// Largest payload (one frame buffer) a recording may declare: 256 MiB.
pub const MAX_PAYLOAD: u64 = 1 << 28;
/// Largest header body.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Longest string in a header.
pub const MAX_STRING: usize = 4096;
/// Most keys in a header.
pub const MAX_KEYS: usize = 64;
/// Largest frame record header.
pub const MAX_FRAME_HEADER: usize = 1024;
/// Most channels a recording declares.
pub const MAX_CHANNELS: usize = 1024;
/// Largest channel schema.
pub const MAX_SCHEMA: usize = 64 * 1024;
/// Largest channel record.
pub const MAX_CHANNEL_RECORD: usize = MAX_SCHEMA + 5 * MAX_STRING;
/// Largest sample: 1 MiB.
pub const MAX_SAMPLE: usize = 1 << 20;
/// Bytes of a sample record's head after its tag: channel, timestamp, length.
pub(crate) const SAMPLE_HEAD: usize = 2 + 8 + 4;

pub(crate) const TAG_END: u8 = 0;
pub(crate) const TAG_FRAME: u8 = 1;
pub(crate) const TAG_CHANNEL: u8 = 2;
pub(crate) const TAG_SAMPLE: u8 = 3;

/// What a recording is of: the device and backend, the frame format, the frame rate when known,
/// and the size of one frame buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingHeader {
    /// The device's display name.
    pub device: String,
    /// The device's identity keys (names, serial numbers), in the producer's order.
    pub keys: Vec<String>,
    /// The backend that produced the frames.
    pub backend: String,
    /// The format of the stream (per-frame formats are in each record).
    pub format: MediaFormat,
    /// The frame interval, when the producer had one.
    pub interval: Option<Interval>,
    /// Bytes per frame buffer: at least the span of every frame recorded.
    pub payload: u64,
}

/// Checks a payload size against [`MAX_PAYLOAD`].
pub(crate) fn check_payload(payload: u64) -> Result<(), RecordError> {
    if payload == 0 {
        return Err(RecordError::Corrupt("zero payload"));
    }
    if payload > MAX_PAYLOAD {
        return Err(RecordError::TooLarge("payload"));
    }
    Ok(())
}

fn put_str(out: &mut Vec<u8>, s: &str) -> Result<(), RecordError> {
    if s.len() > MAX_STRING {
        return Err(RecordError::TooLarge("string"));
    }
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn color_code(color: ColorSpace) -> u8 {
    color.code()
}

fn color_from(code: u8) -> Result<ColorSpace, RecordError> {
    Ok(match code {
        0 => ColorSpace::Srgb,
        1 => ColorSpace::Bt709,
        2 => ColorSpace::Bt2020,
        3 => ColorSpace::Unknown,
        4 => ColorSpace::Bt601,
        _ => return Err(RecordError::Corrupt("unknown colour space")),
    })
}

fn range_code(range: ColorRange) -> u8 {
    match range {
        ColorRange::Full => 0,
        ColorRange::Limited => 1,
        ColorRange::Unknown => 2,
    }
}

fn range_from(code: u8) -> Result<ColorRange, RecordError> {
    Ok(match code {
        0 => ColorRange::Full,
        1 => ColorRange::Limited,
        2 => ColorRange::Unknown,
        _ => return Err(RecordError::Corrupt("unknown colour range")),
    })
}

fn put_format(out: &mut Vec<u8>, format: &MediaFormat) {
    out.extend_from_slice(&format.code.to_u32().to_le_bytes());
    out.extend_from_slice(&format.resolution.width.get().to_le_bytes());
    out.extend_from_slice(&format.resolution.height.get().to_le_bytes());
    out.push(color_code(format.color));
    out.push(range_code(format.range));
}

fn get_format(p: &mut Parser<'_>) -> Result<MediaFormat, RecordError> {
    let code = FourCc::from(p.u32()?);
    let width = p.u32()?;
    let height = p.u32()?;
    let resolution =
        Resolution::new(width, height).ok_or(RecordError::Corrupt("zero resolution"))?;
    let color = color_from(p.u8()?)?;
    let range = range_from(p.u8()?)?;
    Ok(MediaFormat::new(code, resolution, color, range))
}

pub(crate) fn encode_header(header: &RecordingHeader) -> Result<Vec<u8>, RecordError> {
    check_payload(header.payload)?;
    if header.keys.len() > MAX_KEYS {
        return Err(RecordError::TooLarge("device keys"));
    }
    let mut out = Vec::with_capacity(128);
    put_str(&mut out, &header.device)?;
    out.extend_from_slice(&(header.keys.len() as u16).to_le_bytes());
    for key in &header.keys {
        put_str(&mut out, key)?;
    }
    put_str(&mut out, &header.backend)?;
    put_format(&mut out, &header.format);
    match header.interval {
        Some(interval) => {
            out.push(1);
            out.extend_from_slice(&interval.numerator.get().to_le_bytes());
            out.extend_from_slice(&interval.denominator.get().to_le_bytes());
        }
        None => out.push(0),
    }
    out.extend_from_slice(&header.payload.to_le_bytes());
    if out.len() > MAX_HEADER_BYTES {
        return Err(RecordError::TooLarge("header"));
    }
    Ok(out)
}

pub(crate) fn decode_header(body: &[u8]) -> Result<RecordingHeader, RecordError> {
    let mut p = Parser::new(body);
    let device = string(&mut p)?;
    let key_count = usize::from(p.u16()?);
    if key_count > MAX_KEYS {
        return Err(RecordError::TooLarge("device keys"));
    }
    let mut keys = Vec::with_capacity(key_count);
    for _ in 0..key_count {
        keys.push(string(&mut p)?);
    }
    let backend = string(&mut p)?;
    let format = get_format(&mut p)?;
    let interval = match p.u8()? {
        0 => None,
        1 => {
            let numerator =
                NonZeroU32::new(p.u32()?).ok_or(RecordError::Corrupt("zero interval numerator"))?;
            let denominator = NonZeroU32::new(p.u32()?)
                .ok_or(RecordError::Corrupt("zero interval denominator"))?;
            Some(Interval {
                numerator,
                denominator,
            })
        }
        _ => return Err(RecordError::Corrupt("interval flag")),
    };
    let payload = p.u64()?;
    check_payload(payload)?;
    p.finish()?;
    Ok(RecordingHeader {
        device,
        keys,
        backend,
        format,
        interval,
        payload,
    })
}

fn string(p: &mut Parser<'_>) -> Result<String, RecordError> {
    let len = usize::from(p.u16()?);
    if len > MAX_STRING {
        return Err(RecordError::TooLarge("string"));
    }
    String::from_utf8(p.take(len)?.to_vec())
        .map_err(|_| RecordError::Corrupt("string is not UTF-8"))
}

fn put_meta(out: &mut Vec<u8>, meta: &FrameMetadata) {
    for value in &meta.controls {
        match *value {
            ControlValue::Auto => out.push(0),
            ControlValue::Manual(v) => {
                out.push(1);
                out.extend_from_slice(&v.to_le_bytes());
            }
            ControlValue::Flag(flag) => {
                out.push(2);
                out.push(u8::from(flag));
            }
            ControlValue::Crop(None) => out.extend_from_slice(&[3, 0]),
            ControlValue::Crop(Some(rect)) => {
                out.extend_from_slice(&[3, 1]);
                for v in [rect.x, rect.y, rect.width, rect.height] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
    }
    out.extend_from_slice(&meta.sensor_timestamp_ns.to_le_bytes());
    out.extend_from_slice(&meta.exposure_us.to_le_bytes());
    out.extend_from_slice(&meta.analogue_gain_milli.to_le_bytes());
    out.extend_from_slice(&meta.digital_gain_milli.to_le_bytes());
    match meta.lux {
        Some(lux) => {
            out.push(1);
            out.extend_from_slice(&lux.to_bits().to_le_bytes());
        }
        None => out.push(0),
    }
    match meta.colour_temperature_k {
        Some(k) => {
            out.push(1);
            out.extend_from_slice(&k.to_le_bytes());
        }
        None => out.push(0),
    }
}

fn get_meta(p: &mut Parser<'_>) -> Result<FrameMetadata, RecordError> {
    let mut controls = [ControlValue::Auto; Control::COUNT];
    for slot in controls.iter_mut() {
        *slot = match p.u8()? {
            0 => ControlValue::Auto,
            1 => ControlValue::Manual(p.u32()?),
            2 => match p.u8()? {
                0 => ControlValue::Flag(false),
                1 => ControlValue::Flag(true),
                _ => return Err(RecordError::Corrupt("flag value")),
            },
            3 => match p.u8()? {
                0 => ControlValue::Crop(None),
                1 => ControlValue::Crop(Some(Rect {
                    x: p.u32()?,
                    y: p.u32()?,
                    width: p.u32()?,
                    height: p.u32()?,
                })),
                _ => return Err(RecordError::Corrupt("crop presence")),
            },
            _ => return Err(RecordError::Corrupt("control value kind")),
        };
    }
    let sensor_timestamp_ns = p.u64()?;
    let exposure_us = p.u32()?;
    let analogue_gain_milli = p.u32()?;
    let digital_gain_milli = p.u32()?;
    let lux = match p.u8()? {
        0 => None,
        1 => Some(f32::from_bits(p.u32()?)),
        _ => return Err(RecordError::Corrupt("lux presence")),
    };
    let colour_temperature_k = match p.u8()? {
        0 => None,
        1 => Some(p.u32()?),
        _ => return Err(RecordError::Corrupt("colour temperature presence")),
    };
    Ok(FrameMetadata {
        controls,
        sensor_timestamp_ns,
        exposure_us,
        analogue_gain_milli,
        digital_gain_milli,
        lux,
        colour_temperature_k,
        achieved_fps_milli: 0,
    })
}

/// The frame record's header bytes (identity, format, layout and metadata), into `out`.
pub(crate) fn encode_frame_header(
    out: &mut Vec<u8>,
    entry: &FrameEntry,
    meta: &FrameMetadata,
) -> Result<(), RecordError> {
    out.clear();
    out.extend_from_slice(&entry.id.to_le_bytes());
    out.extend_from_slice(&entry.timestamp_ns.to_le_bytes());
    put_format(out, &entry.format);
    let planes: &[PlaneLayout] = &entry.layouts;
    if planes.is_empty() || planes.len() > MAX_PLANES {
        return Err(RecordError::Frame("a frame has one to three planes".into()));
    }
    out.push(planes.len() as u8);
    for plane in planes {
        out.extend_from_slice(&(plane.offset as u64).to_le_bytes());
        out.extend_from_slice(&(plane.stride as u64).to_le_bytes());
        out.extend_from_slice(&(plane.len as u64).to_le_bytes());
    }
    put_meta(out, meta);
    if out.len() > MAX_FRAME_HEADER {
        return Err(RecordError::TooLarge("frame header"));
    }
    Ok(())
}

/// The entry and metadata of a frame record, with every plane checked against `payload`.
pub(crate) fn decode_frame_header(
    buf: &[u8],
    payload: u64,
) -> Result<(FrameEntry, FrameMetadata), RecordError> {
    let mut p = Parser::new(buf);
    let id = p.u64()?;
    let timestamp_ns = p.u64()?;
    let format = get_format(&mut p)?;
    let count = usize::from(p.u8()?);
    if count == 0 || count > MAX_PLANES {
        return Err(RecordError::Corrupt("plane count"));
    }
    let mut planes = [PlaneLayout::default(); MAX_PLANES];
    for plane in planes.iter_mut().take(count) {
        let offset = to_usize(p.u64()?)?;
        let stride = to_usize(p.u64()?)?;
        let len = to_usize(p.u64()?)?;
        *plane = PlaneLayout::new(offset, stride, len);
    }
    let layouts = Layouts::new(&planes[..count]).ok_or(RecordError::Corrupt("plane count"))?;
    for plane in layouts.iter() {
        let end = plane
            .end()
            .ok_or(RecordError::Corrupt("plane end overflows"))?;
        if end as u64 > payload {
            return Err(RecordError::Corrupt("plane runs past the frame buffer"));
        }
    }
    let meta = get_meta(&mut p)?;
    p.finish()?;
    Ok((FrameEntry::new(id, timestamp_ns, format, layouts), meta))
}

/// A channel record's bytes, into `out`.
pub(crate) fn encode_channel(out: &mut Vec<u8>, channel: &Channel) -> Result<(), RecordError> {
    let spec = &channel.spec;
    if spec.schema.len() > MAX_SCHEMA {
        return Err(RecordError::TooLarge("channel schema"));
    }
    out.clear();
    out.extend_from_slice(&channel.id.0.to_le_bytes());
    put_str(out, &spec.name)?;
    put_str(out, &spec.encoding)?;
    put_str(out, &spec.schema_name)?;
    put_str(out, &spec.schema_encoding)?;
    out.extend_from_slice(&(spec.schema.len() as u32).to_le_bytes());
    out.extend_from_slice(&spec.schema);
    out.push(spec.clock.code());
    out.extend_from_slice(&channel.declared.monotonic_ns.to_le_bytes());
    out.extend_from_slice(&channel.declared.boottime_ns.to_le_bytes());
    Ok(())
}

/// A channel record's declaration.
pub(crate) fn decode_channel(buf: &[u8]) -> Result<Channel, RecordError> {
    let mut p = Parser::new(buf);
    let id = ChannelId(p.u16()?);
    let name = string(&mut p)?;
    let encoding = string(&mut p)?;
    let schema_name = string(&mut p)?;
    let schema_encoding = string(&mut p)?;
    let schema_len = p.u32()? as usize;
    if schema_len > MAX_SCHEMA {
        return Err(RecordError::TooLarge("channel schema"));
    }
    let schema = p.take(schema_len)?.to_vec();
    let clock = SourceClock::from_code(p.u8()?).ok_or(RecordError::Corrupt("channel clock"))?;
    let declared = ClockPair {
        monotonic_ns: p.u64()?,
        boottime_ns: p.u64()?,
    };
    p.finish()?;
    Ok(Channel {
        id,
        spec: ChannelSpec {
            name,
            encoding,
            schema_name,
            schema_encoding,
            schema,
            clock,
        },
        declared,
    })
}

/// A sample record's head: channel, timestamp and length.
pub(crate) fn encode_sample_head(
    channel: ChannelId,
    timestamp_ns: u64,
    len: u32,
) -> [u8; SAMPLE_HEAD] {
    let mut head = [0u8; SAMPLE_HEAD];
    head[..2].copy_from_slice(&channel.0.to_le_bytes());
    head[2..10].copy_from_slice(&timestamp_ns.to_le_bytes());
    head[10..].copy_from_slice(&len.to_le_bytes());
    head
}

/// A sample record's channel, timestamp and length, the length checked against [`MAX_SAMPLE`].
pub(crate) fn decode_sample_head(
    head: &[u8; SAMPLE_HEAD],
) -> Result<(ChannelId, u64, usize), RecordError> {
    let mut p = Parser::new(head);
    let channel = ChannelId(p.u16()?);
    let timestamp_ns = p.u64()?;
    let len = p.u32()? as usize;
    if len > MAX_SAMPLE {
        return Err(RecordError::TooLarge("sample"));
    }
    Ok((channel, timestamp_ns, len))
}

fn to_usize(v: u64) -> Result<usize, RecordError> {
    usize::try_from(v).map_err(|_| RecordError::TooLarge("plane offset or length"))
}

/// Reads the file's start: magic, version, header, checksum, and returns the header and the
/// version. Refuses anything that is not a complete, checksummed header of a version this build
/// reads ([`MIN_VERSION`] to [`VERSION`]).
pub(crate) fn read_header(source: &mut impl Read) -> Result<(RecordingHeader, u16), RecordError> {
    let mut fixed = [0u8; 14];
    if !crate::replay::read_full(source, &mut fixed)? {
        return Err(RecordError::Corrupt("truncated header"));
    }
    if fixed[..8] != MAGIC[..] {
        return Err(RecordError::NotARecording);
    }
    let version = u16::from_le_bytes([fixed[8], fixed[9]]);
    if !(MIN_VERSION..=VERSION).contains(&version) {
        return Err(RecordError::UnsupportedVersion(version));
    }
    let len = u32::from_le_bytes([fixed[10], fixed[11], fixed[12], fixed[13]]) as usize;
    if len > MAX_HEADER_BYTES {
        return Err(RecordError::TooLarge("header"));
    }
    let mut body = vec![0u8; len];
    let mut crc = [0u8; 4];
    if !crate::replay::read_full(source, &mut body)? || !crate::replay::read_full(source, &mut crc)?
    {
        return Err(RecordError::Corrupt("truncated header"));
    }
    if u32::from_le_bytes(crc) != crc32(&body) {
        return Err(RecordError::Corrupt("header checksum"));
    }
    Ok((decode_header(&body)?, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> RecordingHeader {
        RecordingHeader {
            device: "cam".into(),
            keys: vec!["a".into(), "b".into()],
            backend: "native".into(),
            format: MediaFormat::with_defaults(FourCc::GREY, Resolution::new(64, 32).unwrap()),
            interval: Some(Interval::from_fps(30).unwrap()),
            payload: 4096,
        }
    }

    #[test]
    fn header_round_trips() {
        let bytes = encode_header(&header()).unwrap();
        assert_eq!(decode_header(&bytes).unwrap(), header());
    }

    #[test]
    fn channel_round_trips() {
        let channel = Channel {
            id: ChannelId(3),
            spec: ChannelSpec {
                name: "/imu".into(),
                encoding: "json".into(),
                schema_name: "Imu".into(),
                schema_encoding: "jsonschema".into(),
                schema: b"{}".to_vec(),
                clock: SourceClock::Boottime,
            },
            declared: ClockPair {
                monotonic_ns: 7,
                boottime_ns: 9,
            },
        };
        let mut bytes = Vec::new();
        encode_channel(&mut bytes, &channel).unwrap();
        assert_eq!(decode_channel(&bytes).unwrap(), channel);
        let head = encode_sample_head(ChannelId(3), 42, 24);
        assert_eq!(decode_sample_head(&head).unwrap(), (ChannelId(3), 42, 24));
        let big = encode_sample_head(ChannelId(0), 0, MAX_SAMPLE as u32 + 1);
        assert!(matches!(
            decode_sample_head(&big),
            Err(RecordError::TooLarge("sample"))
        ));
    }

    #[test]
    fn payload_limits() {
        let mut h = header();
        h.payload = 0;
        assert!(encode_header(&h).is_err());
        h.payload = MAX_PAYLOAD + 1;
        assert!(matches!(
            encode_header(&h),
            Err(RecordError::TooLarge("payload"))
        ));
    }
}
