//! The experimental `.styxrec` format (feature `replay-styxrec`), version 1: a compact
//! Styx-only alternative to MCAP. All integers are little-endian.
//!
//! ```text
//! file    = "STYXREC1" version:u16 header frame* end
//! header  = device:str key_count:u16 key:str* backend:str format interval?
//! frame   = 0x01 body
//! end     = 0x00                       (absent, or after a partial frame, if cut short)
//! body    = format timestamp:u64 clock:u8 backend_meta crop? timing
//!           payload_kind:u8 len:u32 bytes companion_count:u8 companion*
//! companion = 0x01 level:u8 body       (pyramid level)
//! format  = fourcc:u32 width:u32 height:u32 color:u8
//! str     = len:u16 utf8
//! T?      = 0x00 | 0x01 T
//! ```
//!
//! Raw frames store their visible rows tightly packed, plane after plane (strides are not
//! kept); compressed frames store the bitstream as captured.

use std::io::{self, Read, Write};
use std::time::Duration;

use styx_core::prelude::*;

use super::{RecordingHeader, ReplayError, frame_from_payload};
use crate::DeviceIdentity;

pub(crate) const MAGIC: &[u8; 8] = b"STYXREC1";
const VERSION: u16 = 1;
const TAG_END: u8 = 0;
const TAG_FRAME: u8 = 1;
const COMPANION_PYRAMID: u8 = 1;
const PAYLOAD_VISIBLE: u8 = 0;
const PAYLOAD_BITSTREAM: u8 = 1;

pub(crate) fn write_header(w: &mut impl Write, header: &RecordingHeader) -> io::Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    write_str(w, &header.device.display)?;
    w.write_all(&(header.device.keys.len().min(u16::MAX as usize) as u16).to_le_bytes())?;
    for key in header.device.keys.iter().take(u16::MAX as usize) {
        write_str(w, key)?;
    }
    write_str(w, &header.backend)?;
    write_format(w, &header.format)?;
    match header.interval {
        Some(interval) => {
            w.write_all(&[1])?;
            w.write_all(&interval.numerator.get().to_le_bytes())?;
            w.write_all(&interval.denominator.get().to_le_bytes())
        }
        None => w.write_all(&[0]),
    }
}

pub(crate) fn read_header(r: &mut impl Read) -> Result<RecordingHeader, ReplayError> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(ReplayError::NotARecording);
    }
    let version = read_u16(r)?;
    if version != VERSION {
        return Err(ReplayError::UnsupportedVersion(version));
    }
    let display = read_str(r)?;
    let keys = (0..read_u16(r)?)
        .map(|_| read_str(r))
        .collect::<Result<Vec<_>, _>>()?;
    let backend = read_str(r)?;
    let format = read_format(r)?;
    let interval = match read_u8(r)? {
        0 => None,
        _ => {
            let numerator = read_u32(r)?;
            let denominator = read_u32(r)?;
            Some(Interval {
                numerator: nonzero(numerator)?,
                denominator: nonzero(denominator)?,
            })
        }
    };
    Ok(RecordingHeader {
        device: DeviceIdentity { display, keys },
        backend,
        format,
        interval,
    })
}

pub(crate) fn write_frame(w: &mut impl Write, frame: &FrameLease) -> Result<(), ReplayError> {
    w.write_all(&[TAG_FRAME])?;
    write_body(w, frame)
}

pub(crate) fn write_end(w: &mut impl Write) -> io::Result<()> {
    w.write_all(&[TAG_END])
}

/// The next frame, or `None` at the end of the recording (including a recording cut short
/// between frames). `offset` is added to the timestamps of the frame and its companions.
pub(crate) fn read_frame(
    r: &mut impl Read,
    offset: u64,
) -> Result<Option<FrameLease>, ReplayError> {
    let mut tag = [0u8; 1];
    match r.read(&mut tag)? {
        0 => return Ok(None),
        _ if tag[0] == TAG_END => return Ok(None),
        _ if tag[0] == TAG_FRAME => {}
        _ => return Err(ReplayError::Corrupt("unknown record tag")),
    }
    match read_body(r, offset, true) {
        // Cut short mid-frame (e.g. the process died while recording): end at the last
        // complete frame.
        Err(ReplayError::Io(err)) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        other => other.map(Some),
    }
}

fn write_body(w: &mut impl Write, frame: &FrameLease) -> Result<(), ReplayError> {
    let meta = frame.meta();
    write_format(w, &meta.format)?;
    w.write_all(&meta.timestamp.to_le_bytes())?;
    w.write_all(&[clock_tag(meta.clock)])?;
    write_backend(w, meta.backend.as_ref())?;
    match meta.crop {
        Some(rect) => {
            w.write_all(&[1])?;
            for v in [rect.x, rect.y, rect.width, rect.height] {
                w.write_all(&v.to_le_bytes())?;
            }
        }
        None => w.write_all(&[0])?,
    }
    let t = meta.timing;
    for d in [t.sensor_to_capture, t.decode, t.transform, t.hook, t.encode] {
        write_opt_duration(w, d)?;
    }
    if meta.format.code.layout_info().compressed {
        let plane = frame
            .planes()
            .into_iter()
            .next()
            .ok_or(ReplayError::EmptyFrame)?;
        w.write_all(&[PAYLOAD_BITSTREAM])?;
        write_bytes(w, plane.data())?;
    } else {
        let bytes = frame
            .to_visible_vec()
            .map_err(|e| ReplayError::Frame(e.to_string()))?;
        w.write_all(&[PAYLOAD_VISIBLE])?;
        write_bytes(w, &bytes)?;
    }
    let companions: Vec<_> = frame.companions().collect();
    w.write_all(&[companions.len().min(u8::MAX as usize) as u8])?;
    for (kind, companion) in companions.into_iter().take(u8::MAX as usize) {
        let CompanionKind::Pyramid { level } = kind;
        w.write_all(&[COMPANION_PYRAMID, level])?;
        write_body(w, companion)?;
    }
    Ok(())
}

/// A frame body; companions only at the top level (the recorder never nests them).
fn read_body(r: &mut impl Read, offset: u64, top_level: bool) -> Result<FrameLease, ReplayError> {
    let format = read_format(r)?;
    let timestamp = read_u64(r)?.saturating_add(offset);
    let clock = clock_from_tag(read_u8(r)?)?;
    let backend = read_backend(r)?;
    let crop = match read_u8(r)? {
        0 => None,
        _ => Some(FrameRect::new(
            read_u32(r)?,
            read_u32(r)?,
            read_u32(r)?,
            read_u32(r)?,
        )),
    };
    let timing = FrameTiming {
        sensor_to_capture: read_opt_duration(r)?,
        decode: read_opt_duration(r)?,
        transform: read_opt_duration(r)?,
        hook: read_opt_duration(r)?,
        encode: read_opt_duration(r)?,
    };
    let payload_kind = read_u8(r)?;
    let len = read_u32(r)? as usize;
    // Grow with the data actually read, so a corrupt length cannot allocate gigabytes.
    let mut bytes = Vec::with_capacity(len.min(1 << 20));
    if r.by_ref().take(len as u64).read_to_end(&mut bytes)? < len {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
    }
    let mut frame = match payload_kind {
        PAYLOAD_VISIBLE => frame_from_payload(format, timestamp, &bytes, false)?,
        PAYLOAD_BITSTREAM => frame_from_payload(format, timestamp, &bytes, true)?,
        _ => return Err(ReplayError::Corrupt("unknown payload kind")),
    };
    let meta = frame.meta_mut();
    meta.clock = clock;
    meta.backend = backend;
    meta.crop = crop;
    meta.timing = timing;
    let companions = read_u8(r)?;
    if companions > 0 && !top_level {
        return Err(ReplayError::Corrupt("nested companion frames"));
    }
    for _ in 0..companions {
        let kind = match (read_u8(r)?, read_u8(r)?) {
            (COMPANION_PYRAMID, level) => CompanionKind::Pyramid { level },
            _ => return Err(ReplayError::Corrupt("unknown companion kind")),
        };
        let companion = read_body(r, offset, false)?;
        frame = frame
            .with_companion(kind, companion)
            .map_err(|e| ReplayError::Frame(e.to_string()))?;
    }
    Ok(frame)
}

fn write_backend(w: &mut impl Write, backend: Option<&BackendFrameMeta>) -> io::Result<()> {
    match backend {
        None => w.write_all(&[0]),
        Some(BackendFrameMeta::V4l2(m)) => {
            w.write_all(&[1])?;
            for v in [m.sequence, m.bytes_used, m.field, m.flags] {
                w.write_all(&v.to_le_bytes())?;
            }
            w.write_all(&[m.zero_copy as u8])
        }
        Some(BackendFrameMeta::Libcamera(m)) => {
            w.write_all(&[2])?;
            w.write_all(&m.sequence.to_le_bytes())?;
            write_str(w, m.buffer_memory)
        }
    }
}

fn read_backend(r: &mut impl Read) -> Result<Option<BackendFrameMeta>, ReplayError> {
    Ok(match read_u8(r)? {
        0 => None,
        1 => Some(BackendFrameMeta::V4l2(V4l2FrameMeta {
            sequence: read_u32(r)?,
            bytes_used: read_u32(r)?,
            field: read_u32(r)?,
            flags: read_u32(r)?,
            zero_copy: read_u8(r)? != 0,
        })),
        2 => Some(BackendFrameMeta::Libcamera(LibcameraFrameMeta {
            sequence: read_u32(r)?,
            buffer_memory: match read_str(r)?.as_str() {
                "dma-heap" => "dma-heap",
                "libcamera-allocator" => "libcamera-allocator",
                _ => "recorded",
            },
        })),
        _ => return Err(ReplayError::Corrupt("unknown backend metadata")),
    })
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

fn write_format(w: &mut impl Write, format: &MediaFormat) -> io::Result<()> {
    w.write_all(&format.code.to_u32().to_le_bytes())?;
    w.write_all(&format.resolution.width.get().to_le_bytes())?;
    w.write_all(&format.resolution.height.get().to_le_bytes())?;
    w.write_all(&[match format.color {
        ColorSpace::Srgb => 0,
        ColorSpace::Bt709 => 1,
        ColorSpace::Bt2020 => 2,
        ColorSpace::Unknown => 3,
    }])
}

fn read_format(r: &mut impl Read) -> Result<MediaFormat, ReplayError> {
    let code = FourCc::from(read_u32(r)?);
    let resolution = Resolution::new(read_u32(r)?, read_u32(r)?)
        .ok_or(ReplayError::Corrupt("zero resolution"))?;
    let color = match read_u8(r)? {
        0 => ColorSpace::Srgb,
        1 => ColorSpace::Bt709,
        2 => ColorSpace::Bt2020,
        _ => ColorSpace::Unknown,
    };
    Ok(MediaFormat::new(code, resolution, color))
}

fn write_opt_duration(w: &mut impl Write, d: Option<Duration>) -> io::Result<()> {
    match d {
        Some(d) => {
            w.write_all(&[1])?;
            w.write_all(&(d.as_nanos().min(u64::MAX as u128) as u64).to_le_bytes())
        }
        None => w.write_all(&[0]),
    }
}

fn read_opt_duration(r: &mut impl Read) -> Result<Option<Duration>, ReplayError> {
    Ok(match read_u8(r)? {
        0 => None,
        _ => Some(Duration::from_nanos(read_u64(r)?)),
    })
}

fn write_bytes(w: &mut impl Write, bytes: &[u8]) -> Result<(), ReplayError> {
    let len = u32::try_from(bytes.len()).map_err(|_| ReplayError::Corrupt("frame over 4 GiB"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    let bytes = &s.as_bytes()[..s.len().min(u16::MAX as usize)];
    w.write_all(&(bytes.len() as u16).to_le_bytes())?;
    w.write_all(bytes)
}

fn read_str(r: &mut impl Read) -> Result<String, ReplayError> {
    let mut bytes = vec![0u8; read_u16(r)? as usize];
    r.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| ReplayError::Corrupt("invalid utf-8"))
}

fn nonzero(v: u32) -> Result<std::num::NonZeroU32, ReplayError> {
    std::num::NonZeroU32::new(v).ok_or(ReplayError::Corrupt("zero interval"))
}

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
