//! Message layout, little-endian. Every message starts with the magic, the version and a kind.
//!
//! Server to client:
//! - Frame: id, then the frame: format (fourcc, width, height, colour), timestamp and clock,
//!   whether it is an inter-coded packet, crop, plane layouts and backing (a memfd's length, or each dma-buf plane's offset and
//!   length), then its companions, each a frame of its own. Descriptors are attached in the same
//!   order: one per memfd, one per dma-buf plane.
//! - Accept / Reject (camera service): the consumer's plan, or why none fits.
//! - Cameras (camera service): the cameras it serves, in answer to List.
//!
//! Client to server:
//! - Release: the id of a frame the client dropped.
//! - Request (camera service): the consumer's `FrameRequirements`, and optionally which camera.
//! - List (camera service): which cameras it serves.
//! - Roi: a new region of interest, or none.

use styx_core::prelude::*;

use super::IpcError;

const MAGIC: u32 = u32::from_le_bytes(*b"STYX");
const VERSION: u16 = 3;
const KIND_FRAME: u16 = 1;
const KIND_RELEASE: u16 = 2;
const KIND_REQUEST: u16 = 3;
const KIND_ACCEPT: u16 = 4;
const KIND_REJECT: u16 = 5;
const KIND_ROI: u16 = 6;
const KIND_LIST: u16 = 7;
const KIND_CAMERAS: u16 = 8;
/// Most cameras a camera list carries.
const MAX_CAMERAS: usize = 64;
/// Most identity keys per camera, formats per request and names in a forbid list.
const MAX_KEYS: u8 = 8;
const MAX_NAMES: u8 = 16;
const MAX_PLANES: usize = 4;
const MAX_COMPANIONS: usize = 3;
/// Longest text in Accept and Reject messages.
const MAX_TEXT: usize = 3072;

pub(super) enum WireBacking {
    Memfd {
        len: usize,
    },
    /// Offset and length of each plane in its dma-buf.
    Dmabuf(Vec<(usize, usize)>),
}

impl WireBacking {
    /// Descriptors attached for this backing.
    pub(super) fn fd_count(&self) -> usize {
        match self {
            Self::Memfd { .. } => 1,
            Self::Dmabuf(planes) => planes.len(),
        }
    }
}

pub(super) struct WireFrame {
    pub meta: FrameMeta,
    pub layouts: Vec<PlaneLayout>,
    pub backing: WireBacking,
    pub companions: Vec<(CompanionKind, WireFrame)>,
}

/// A message from a client.
pub(super) enum ClientMessage {
    Release(u64),
    /// Frames meeting these requirements, from the named camera (or the service's first).
    Request(Box<FrameRequirements>, Option<String>),
    Roi(Option<FrameRect>),
    List,
}

/// A message from a server.
pub(super) enum ServerMessage {
    Frame(u64, Box<WireFrame>),
    Accept(String),
    Reject(String),
    Cameras(Vec<CameraInfo>),
}

/// A camera a [`CameraService`](super::CameraService) serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraInfo {
    /// Its display name (e.g. `ov9782` or `046d:0825`); requests may name it by any part.
    pub name: String,
    /// Identity keys (USB vendor/product, bus path, sensor path); requests may name one.
    pub keys: Vec<String>,
    /// Clients are receiving frames from it.
    pub in_use: bool,
}

struct Writer(Vec<u8>);

impl Writer {
    fn new(kind: u16) -> Self {
        let mut w = Self(Vec::with_capacity(192));
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
    fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }
    fn text(&mut self, v: &str) {
        let mut end = v.len().min(MAX_TEXT);
        while !v.is_char_boundary(end) {
            end -= 1;
        }
        self.u16(end as u16);
        self.0.extend_from_slice(&v.as_bytes()[..end]);
    }
    fn opt<T>(&mut self, v: Option<T>, mut f: impl FnMut(&mut Self, T)) {
        self.bool(v.is_some());
        if let Some(v) = v {
            f(self, v);
        }
    }
    fn size(&mut self, (w, h): (u32, u32)) {
        self.u32(w);
        self.u32(h);
    }
    fn rect(&mut self, r: FrameRect) {
        for v in [r.x, r.y, r.width, r.height] {
            self.u32(v);
        }
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
    fn bool(&mut self) -> Result<bool, IpcError> {
        Ok(self.u8()? != 0)
    }
    fn text(&mut self) -> Result<String, IpcError> {
        let len = usize::from(self.u16()?);
        if len > MAX_TEXT {
            return Err(IpcError::Malformed("text too long"));
        }
        if self.0.len() < len {
            return Err(IpcError::Malformed("message too short"));
        }
        let (text, rest) = self.0.split_at(len);
        self.0 = rest;
        String::from_utf8(text.to_vec()).map_err(|_| IpcError::Malformed("text is not UTF-8"))
    }
    fn opt<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, IpcError>,
    ) -> Result<Option<T>, IpcError> {
        if self.bool()? {
            f(self).map(Some)
        } else {
            Ok(None)
        }
    }
    fn size(&mut self) -> Result<(u32, u32), IpcError> {
        Ok((self.u32()?, self.u32()?))
    }
    fn rect(&mut self) -> Result<FrameRect, IpcError> {
        Ok(FrameRect::new(
            self.u32()?,
            self.u32()?,
            self.u32()?,
            self.u32()?,
        ))
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

fn write_frame(w: &mut Writer, frame: &WireFrame) {
    let format = frame.meta.format;
    w.u32(format.code.to_u32());
    w.u32(format.resolution.width.get());
    w.u32(format.resolution.height.get());
    w.u8(color_tag(format.color));
    w.u64(frame.meta.timestamp);
    w.u8(clock_tag(frame.meta.clock));
    w.bool(frame.meta.delta);
    w.opt(frame.meta.crop, Writer::rect);
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
    w.u8(frame.companions.len() as u8);
    for (kind, companion) in &frame.companions {
        match kind {
            CompanionKind::Pyramid { level } => {
                w.u8(1);
                w.u8(*level);
            }
            CompanionKind::Scaled => {
                w.u8(2);
                w.u8(0);
            }
        }
        write_frame(w, companion);
    }
}

fn read_frame(r: &mut Reader<'_>, top_level: bool) -> Result<WireFrame, IpcError> {
    let code = FourCc::new(r.u32()?.to_le_bytes());
    let resolution =
        Resolution::new(r.u32()?, r.u32()?).ok_or(IpcError::Malformed("zero-sized frame"))?;
    let color = color_from(r.u8()?);
    let mut meta = FrameMeta::new(MediaFormat::new(code, resolution, color), r.u64()?);
    meta.clock = clock_from(r.u8()?);
    meta.delta = r.bool()?;
    meta.crop = r.opt(Reader::rect)?;
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
    let count = usize::from(r.u8()?);
    if count > MAX_COMPANIONS || (count > 0 && !top_level) {
        return Err(IpcError::Malformed("bad companions"));
    }
    let companions = (0..count)
        .map(|_| {
            let kind = match (r.u8()?, r.u8()?) {
                (1, level) => CompanionKind::Pyramid { level },
                (2, _) => CompanionKind::Scaled,
                _ => return Err(IpcError::Malformed("unknown companion kind")),
            };
            Ok((kind, read_frame(r, false)?))
        })
        .collect::<Result<_, IpcError>>()?;
    Ok(WireFrame {
        meta,
        layouts,
        backing,
        companions,
    })
}

pub(super) fn encode_frame(id: u64, frame: &WireFrame) -> Vec<u8> {
    let mut w = Writer::new(KIND_FRAME);
    w.u64(id);
    write_frame(&mut w, frame);
    w.0
}

pub(super) fn encode_release(id: u64) -> Vec<u8> {
    let mut w = Writer::new(KIND_RELEASE);
    w.u64(id);
    w.0
}

pub(super) fn encode_accept(plan: &str) -> Vec<u8> {
    let mut w = Writer::new(KIND_ACCEPT);
    w.text(plan);
    w.0
}

pub(super) fn encode_reject(reason: &str) -> Vec<u8> {
    let mut w = Writer::new(KIND_REJECT);
    w.text(reason);
    w.0
}

pub(super) fn encode_roi(roi: Option<FrameRect>) -> Vec<u8> {
    let mut w = Writer::new(KIND_ROI);
    w.opt(roi, Writer::rect);
    w.0
}

pub(super) fn encode_list() -> Vec<u8> {
    Writer::new(KIND_LIST).0
}

pub(super) fn encode_cameras(cameras: &[CameraInfo]) -> Vec<u8> {
    let mut w = Writer::new(KIND_CAMERAS);
    w.u8(cameras.len().min(MAX_CAMERAS) as u8);
    for camera in cameras.iter().take(MAX_CAMERAS) {
        w.text(&camera.name);
        w.u8(camera.keys.len().min(usize::from(MAX_KEYS)) as u8);
        for key in camera.keys.iter().take(usize::from(MAX_KEYS)) {
            w.text(key);
        }
        w.bool(camera.in_use);
    }
    w.0
}

fn read_cameras(r: &mut Reader<'_>) -> Result<Vec<CameraInfo>, IpcError> {
    let count = usize::from(r.u8()?);
    if count > MAX_CAMERAS {
        return Err(IpcError::Malformed("too many cameras"));
    }
    (0..count)
        .map(|_| {
            let name = r.text()?;
            let count = r.u8()?;
            if count > MAX_KEYS {
                return Err(IpcError::Malformed("too many identity keys"));
            }
            let keys = (0..count)
                .map(|_| r.text())
                .collect::<Result<_, IpcError>>()?;
            Ok(CameraInfo {
                name,
                keys,
                in_use: r.bool()?,
            })
        })
        .collect()
}

pub(super) fn encode_request(req: &FrameRequirements, camera: Option<&str>) -> Vec<u8> {
    let mut w = Writer::new(KIND_REQUEST);
    w.opt(camera, Writer::text);
    match &req.output {
        OutputFormat::Luma => w.u8(0),
        OutputFormat::Formats(formats) => {
            w.u8(1);
            w.u8(formats.len().min(usize::from(MAX_NAMES)) as u8);
            for code in formats.iter().take(usize::from(MAX_NAMES)) {
                w.u32(code.to_u32());
            }
        }
    }
    w.opt(req.stride_alignment, Writer::usize);
    w.opt(req.pyramid, |w, p| {
        w.u8(p.levels);
        w.u8(match p.source {
            PyramidSource::PreferHardware => 0,
            PyramidSource::HardwareOnly => 1,
            PyramidSource::Software => 2,
        });
    });
    w.opt(req.roi, Writer::rect);
    w.opt(req.min_resolution, Writer::size);
    w.opt(req.max_resolution, Writer::size);
    w.opt(req.output_resolution, Writer::size);
    w.opt(req.min_fps, Writer::u32);
    w.u8(match req.priority {
        Priority::Latency => 0,
        Priority::Throughput => 1,
        Priority::Power => 2,
    });
    w.bool(req.strict);
    let o = &req.overrides;
    w.opt(o.backend.as_deref(), Writer::text);
    w.opt(o.decoder.as_deref(), Writer::text);
    w.u8(o.forbid.len().min(usize::from(MAX_NAMES)) as u8);
    for name in o.forbid.iter().take(usize::from(MAX_NAMES)) {
        w.text(name);
    }
    w.u8(match o.hardware {
        HardwarePolicy::Auto => 0,
        HardwarePolicy::Disabled => 1,
        HardwarePolicy::Required => 2,
    });
    w.opt(o.decode_threads, Writer::usize);
    w.opt(o.queue_depth, Writer::usize);
    w.0
}

fn read_request(r: &mut Reader<'_>) -> Result<FrameRequirements, IpcError> {
    let mut req = match r.u8()? {
        0 => FrameRequirements::luma(),
        1 => {
            let count = r.u8()?;
            if count > MAX_NAMES {
                return Err(IpcError::Malformed("too many formats"));
            }
            let formats = (0..count)
                .map(|_| Ok(FourCc::new(r.u32()?.to_le_bytes())))
                .collect::<Result<Vec<_>, IpcError>>()?;
            FrameRequirements::formats(formats)
        }
        _ => return Err(IpcError::Malformed("unknown output")),
    };
    req.stride_alignment = r.opt(Reader::usize)?;
    req.pyramid = r.opt(|r| {
        Ok(PyramidRequest {
            levels: r.u8()?,
            source: match r.u8()? {
                1 => PyramidSource::HardwareOnly,
                2 => PyramidSource::Software,
                _ => PyramidSource::PreferHardware,
            },
        })
    })?;
    req.roi = r.opt(Reader::rect)?;
    req.min_resolution = r.opt(Reader::size)?;
    req.max_resolution = r.opt(Reader::size)?;
    req.output_resolution = r.opt(Reader::size)?;
    req.min_fps = r.opt(Reader::u32)?;
    req.priority = match r.u8()? {
        1 => Priority::Throughput,
        2 => Priority::Power,
        _ => Priority::Latency,
    };
    req.strict = r.bool()?;
    let o = &mut req.overrides;
    o.backend = r.opt(Reader::text)?;
    o.decoder = r.opt(Reader::text)?;
    let forbidden = r.u8()?;
    if forbidden > MAX_NAMES {
        return Err(IpcError::Malformed("too many forbidden codecs"));
    }
    o.forbid = (0..forbidden)
        .map(|_| r.text())
        .collect::<Result<_, IpcError>>()?;
    o.hardware = match r.u8()? {
        1 => HardwarePolicy::Disabled,
        2 => HardwarePolicy::Required,
        _ => HardwarePolicy::Auto,
    };
    o.decode_threads = r.opt(Reader::usize)?;
    o.queue_depth = r.opt(Reader::usize)?;
    Ok(req)
}

pub(super) fn decode_client(bytes: &[u8]) -> Result<ClientMessage, IpcError> {
    let (mut r, kind) = Reader::start(bytes)?;
    match kind {
        KIND_RELEASE => Ok(ClientMessage::Release(r.u64()?)),
        KIND_REQUEST => {
            let camera = r.opt(Reader::text)?;
            Ok(ClientMessage::Request(
                Box::new(read_request(&mut r)?),
                camera,
            ))
        }
        KIND_LIST => Ok(ClientMessage::List),
        KIND_ROI => Ok(ClientMessage::Roi(r.opt(Reader::rect)?)),
        _ => Err(IpcError::Malformed("unexpected message from a client")),
    }
}

pub(super) fn decode_server(bytes: &[u8]) -> Result<ServerMessage, IpcError> {
    let (mut r, kind) = Reader::start(bytes)?;
    match kind {
        KIND_FRAME => {
            let id = r.u64()?;
            Ok(ServerMessage::Frame(
                id,
                Box::new(read_frame(&mut r, true)?),
            ))
        }
        KIND_ACCEPT => Ok(ServerMessage::Accept(r.text()?)),
        KIND_REJECT => Ok(ServerMessage::Reject(r.text()?)),
        KIND_CAMERAS => Ok(ServerMessage::Cameras(read_cameras(&mut r)?)),
        _ => Err(IpcError::Malformed("unexpected message from a server")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_survive_the_wire() {
        let mut req = FrameRequirements::formats([FourCc::NV12, FourCc::MJPG])
            .output_resolution(320, 180)
            .pyramid(2)
            .roi(FrameRect::new(1, 2, 3, 4))
            .stride_alignment(64);
        req.min_fps = Some(15);
        req.priority = Priority::Power;
        req.overrides.backend = Some("libcamera".into());
        req.overrides.forbid = vec!["ffmpeg".into()];
        req.overrides.queue_depth = Some(3);
        let ClientMessage::Request(back, camera) =
            decode_client(&encode_request(&req, Some("ov9782"))).unwrap()
        else {
            panic!("not a request");
        };
        assert_eq!(*back, req);
        assert_eq!(camera.as_deref(), Some("ov9782"));
        let cameras = vec![CameraInfo {
            name: "ov9782".into(),
            keys: vec!["i2c:ov9782".into()],
            in_use: true,
        }];
        let ServerMessage::Cameras(back) = decode_server(&encode_cameras(&cameras)).unwrap() else {
            panic!("not a camera list");
        };
        assert_eq!(back, cameras);
    }
}
