//! Message layout, little-endian. Every message starts with the magic, the version and a kind.
//!
//! - Frame: id, format (fourcc, width, height, colour), timestamp and clock, crop, plane
//!   layouts, then the backing: a memfd's length, or each dma-buf plane's offset and length (one
//!   descriptor per plane, attached to the message).
//! - Release (client to server): the id of a frame the client dropped.

use styx_core::prelude::*;

use super::IpcError;

const MAGIC: u32 = u32::from_le_bytes(*b"STYX");
const VERSION: u16 = 1;
const KIND_FRAME: u16 = 1;
const KIND_RELEASE: u16 = 2;
const MAX_PLANES: usize = 4;

pub(super) enum WireBacking {
    Memfd {
        len: usize,
    },
    /// Offset and length of each plane in its dma-buf.
    Dmabuf(Vec<(usize, usize)>),
}

pub(super) struct FrameHeader {
    pub id: u64,
    pub meta: FrameMeta,
    pub layouts: Vec<PlaneLayout>,
    pub backing: WireBacking,
}

struct Writer(Vec<u8>);

impl Writer {
    fn new(kind: u16) -> Self {
        let mut w = Self(Vec::with_capacity(160));
        w.u32(MAGIC);
        w.u16(VERSION);
        w.u16(kind);
        w
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn usize(&mut self, v: usize) {
        self.u64(v as u64);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], IpcError> {
        let (head, rest) = self
            .0
            .split_first_chunk::<N>()
            .ok_or(IpcError::Malformed("message too short"))?;
        self.0 = rest;
        Ok(*head)
    }
    fn u8(&mut self) -> Result<u8, IpcError> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, IpcError> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32, IpcError> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, IpcError> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    fn usize(&mut self) -> Result<usize, IpcError> {
        usize::try_from(self.u64()?).map_err(|_| IpcError::Malformed("size out of range"))
    }
    /// Check the magic and version and return the kind.
    fn start(bytes: &[u8]) -> Result<(Reader<'_>, u16), IpcError> {
        let mut r = Reader(bytes);
        if r.u32()? != MAGIC {
            return Err(IpcError::Malformed("not a Styx message"));
        }
        if r.u16()? != VERSION {
            return Err(IpcError::Malformed("unsupported version"));
        }
        let kind = r.u16()?;
        Ok((r, kind))
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

fn color_from(tag: u8) -> ColorSpace {
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

fn clock_from(tag: u8) -> Option<TimestampClock> {
    match tag {
        1 => Some(TimestampClock::Monotonic),
        2 => Some(TimestampClock::Boottime),
        3 => Some(TimestampClock::Realtime),
        4 => Some(TimestampClock::StreamRelative),
        _ => None,
    }
}

pub(super) fn encode_frame(frame: &FrameHeader) -> Vec<u8> {
    let mut w = Writer::new(KIND_FRAME);
    w.u64(frame.id);
    let format = frame.meta.format;
    w.u32(format.code.to_u32());
    w.u32(format.resolution.width.get());
    w.u32(format.resolution.height.get());
    w.u8(color_tag(format.color));
    w.u64(frame.meta.timestamp);
    w.u8(clock_tag(frame.meta.clock));
    match frame.meta.crop {
        Some(crop) => {
            w.u8(1);
            for v in [crop.x, crop.y, crop.width, crop.height] {
                w.u32(v);
            }
        }
        None => w.u8(0),
    }
    w.u8(frame.layouts.len() as u8);
    for layout in &frame.layouts {
        w.usize(layout.offset);
        w.usize(layout.len);
        w.usize(layout.stride);
    }
    match &frame.backing {
        WireBacking::Memfd { len } => {
            w.u8(0);
            w.usize(*len);
        }
        WireBacking::Dmabuf(planes) => {
            w.u8(1);
            w.u8(planes.len() as u8);
            for &(offset, len) in planes {
                w.usize(offset);
                w.usize(len);
            }
        }
    }
    w.0
}

pub(super) fn decode_frame(bytes: &[u8]) -> Result<FrameHeader, IpcError> {
    let (mut r, kind) = Reader::start(bytes)?;
    if kind != KIND_FRAME {
        return Err(IpcError::Malformed("not a frame"));
    }
    let id = r.u64()?;
    let code = FourCc::new(r.u32()?.to_le_bytes());
    let resolution =
        Resolution::new(r.u32()?, r.u32()?).ok_or(IpcError::Malformed("zero-sized frame"))?;
    let color = color_from(r.u8()?);
    let mut meta = FrameMeta::new(MediaFormat::new(code, resolution, color), r.u64()?);
    meta.clock = clock_from(r.u8()?);
    if r.u8()? == 1 {
        meta.crop = Some(FrameRect::new(r.u32()?, r.u32()?, r.u32()?, r.u32()?));
    }
    let planes = usize::from(r.u8()?);
    if planes == 0 || planes > MAX_PLANES {
        return Err(IpcError::Malformed("bad plane count"));
    }
    let layouts = (0..planes)
        .map(|_| {
            Ok(PlaneLayout {
                offset: r.usize()?,
                len: r.usize()?,
                stride: r.usize()?,
            })
        })
        .collect::<Result<Vec<_>, IpcError>>()?;
    let backing = match r.u8()? {
        0 => WireBacking::Memfd { len: r.usize()? },
        1 => {
            let count = usize::from(r.u8()?);
            if count != planes {
                return Err(IpcError::Malformed("bad dma-buf plane count"));
            }
            WireBacking::Dmabuf(
                (0..count)
                    .map(|_| Ok((r.usize()?, r.usize()?)))
                    .collect::<Result<_, IpcError>>()?,
            )
        }
        _ => return Err(IpcError::Malformed("unknown backing")),
    };
    Ok(FrameHeader {
        id,
        meta,
        layouts,
        backing,
    })
}

pub(super) fn encode_release(id: u64) -> Vec<u8> {
    let mut w = Writer::new(KIND_RELEASE);
    w.u64(id);
    w.0
}

pub(super) fn decode_release(bytes: &[u8]) -> Result<u64, IpcError> {
    let (mut r, kind) = Reader::start(bytes)?;
    if kind != KIND_RELEASE {
        return Err(IpcError::Malformed("not a release"));
    }
    r.u64()
}
