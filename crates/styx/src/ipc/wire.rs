//! Message layout, little-endian. Every message starts with the magic, the version and a kind.
//!
//! Server to client:
//! - Frame: id, then the frame: format (fourcc, width, height, colour), timestamp and clock,
//!   whether it is an inter-coded packet, crop, plane layouts and backing (a memfd's length, or each dma-buf plane's offset and
//!   length), then its companions, each a frame of its own. Descriptors are attached in the same
//!   order: one per memfd, one per dma-buf plane.
//! - Accept / Reject (camera service): the consumer's plan, or why none fits.
//! - Cameras (camera service): the cameras it serves, in answer to List.
//! - Metrics (camera service): the format and length of the service's metrics, carried in the
//!   attached memfd, in answer to a metrics request.
//! - ControlReply, ControlEvent (camera service): answers to control requests, and control
//!   changes for connections that subscribed (`controls.rs`).
//!
//! The accept message carries the client's id and token on its camera as a trailer older
//! clients ignore.
//!
//! Frames carry their hops (sensor to send) and releases the client's receive and import times
//! as a trailer older peers ignore (`hops.rs`).
//!
//! Client to server:
//! - Release: the id of a frame the client dropped (and its receive and import times).
//! - Request (camera service): the consumer's `FrameRequest`, and optionally which camera; a
//!   low-priority client adds a priority trailer older services ignore (`request.rs`).
//! - List (camera service): which cameras it serves.
//! - Roi: the regions of interest now (none: the whole frame).
//! - Metrics (camera service): the service's metrics, as JSON (0) or Prometheus text (1).
//! - Control (camera service): set, get or list camera controls, or subscribe to changes.

use styx_core::prelude::*;

mod controls;
mod delivered;
mod hops;
mod request;

pub(super) use self::controls::{
    ControlOp, ControlReply, ControlRequest, MAX_CONTROL_LIST, decode_control_list, encode_control,
    encode_control_event, encode_control_list, encode_control_reply,
};
use self::controls::{
    KIND_CONTROL, KIND_CONTROL_EVENT, KIND_CONTROL_REPLY, read_control, read_control_event,
    read_control_reply,
};
use self::delivered::{read_delivered, write_delivered};
pub(super) use self::hops::ClientHops;
use self::hops::{read_frame_hops, read_release_hops, write_frame_hops};
pub(in crate::ipc) use self::request::encode_request;
use self::request::{read_priority, read_request};
use super::IpcError;
use crate::planner::{Delivered, FrameRequest};

const MAGIC: u32 = u32::from_le_bytes(*b"STYX");
const VERSION: u16 = 8;
const KIND_FRAME: u16 = 1;
const KIND_RELEASE: u16 = 2;
const KIND_REQUEST: u16 = 3;
const KIND_ACCEPT: u16 = 4;
const KIND_REJECT: u16 = 5;
const KIND_ROI: u16 = 6;
const KIND_LIST: u16 = 7;
const KIND_CAMERAS: u16 = 8;
const KIND_METRICS: u16 = 9;
const KIND_METRICS_REPLY: u16 = 10;
/// Most cameras a camera list carries.
const MAX_CAMERAS: usize = 64;
/// Most identity keys per camera, formats per request and names in a forbid list.
const MAX_KEYS: u8 = 8;
const MAX_NAMES: u8 = 16;
const MAX_PLANES: usize = 4;
const MAX_COMPANIONS: usize = 3 + crate::planner::MAX_REGIONS;
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
    /// Whether the sender's memory reads cached on the CPU (it is the same memory here).
    pub cpu_access: CpuAccess,
    pub companions: Vec<(CompanionKind, WireFrame)>,
}

/// A message from a client.
pub(super) enum ClientMessage {
    /// A frame the client dropped, and when it received and imported it.
    Release(u64, Option<ClientHops>),
    /// These frames, from the named camera (or the service's first), at this priority.
    Request(Box<FrameRequest>, Option<String>, super::ClientPriority),
    /// The regions of interest now, region 0 first (empty: the whole frame).
    Roi(Vec<FrameRect>),
    List,
    /// The service's metrics, in this format.
    Metrics(MetricsFormat),
    /// A camera control request (`controls.rs`).
    Control(Box<ControlRequest>),
}

/// How a camera service sends its metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MetricsFormat {
    Json,
    Prometheus,
}

/// A message from a server.
pub(super) enum ServerMessage {
    /// A frame (read with [`decode_frame_into`]).
    Frame,
    /// The plan, as text, what its frames are, and the client's id and token on the camera
    /// (a trailer older services do not send).
    Accept(String, Box<Delivered>, Option<ClientToken>),
    Reject(String),
    Cameras(Vec<CameraInfo>),
    /// Metrics of this format and length in bytes, in the attached memfd.
    Metrics(MetricsFormat, usize),
    /// The answer to the control request of this sequence number.
    ControlReply(u32, ControlReply),
    ControlEvent(crate::ipc::ControlEvent),
}

/// A client's id on its camera (shown to other clients in control events) and its token (kept
/// between the client and the service: it proves the client on control requests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ClientToken {
    pub id: u64,
    pub token: u64,
}

/// Tag of the accept message's client token trailer.
const TOKEN_TAG: u8 = 1;

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
        Self::reuse(Vec::with_capacity(192), kind)
    }

    /// A message of `kind` written into `buf` (cleared first).
    fn reuse(mut buf: Vec<u8>, kind: u16) -> Self {
        buf.clear();
        let mut w = Self(buf);
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
    w.u8(match frame.cpu_access {
        CpuAccess::None => 0,
        CpuAccess::Uncached => 1,
        CpuAccess::Cached => 2,
    });
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
            CompanionKind::Overview => {
                w.u8(3);
                w.u8(0);
            }
            CompanionKind::Region { index } => {
                w.u8(4);
                w.u8(*index);
            }
        }
        write_frame(w, companion);
    }
}

/// A frame's fields into `frame`, its lists reused.
fn read_frame_into(
    r: &mut Reader<'_>,
    frame: &mut WireFrame,
    top_level: bool,
) -> Result<(), IpcError> {
    let code = FourCc::new(r.u32()?.to_le_bytes());
    let resolution =
        Resolution::new(r.u32()?, r.u32()?).ok_or(IpcError::Malformed("zero-sized frame"))?;
    let color = color_from(r.u8()?);
    let mut meta = FrameMeta::new(MediaFormat::new(code, resolution, color), r.u64()?);
    meta.clock = clock_from(r.u8()?);
    meta.delta = r.bool()?;
    frame.cpu_access = match r.u8()? {
        0 => CpuAccess::None,
        1 => CpuAccess::Uncached,
        2 => CpuAccess::Cached,
        _ => return Err(IpcError::Malformed("unknown CPU access")),
    };
    meta.crop = r.opt(Reader::rect)?;
    frame.meta = meta;
    let planes = usize::from(r.u8()?);
    if planes == 0 || planes > MAX_PLANES {
        return Err(IpcError::Malformed("bad plane count"));
    }
    frame.layouts.clear();
    for _ in 0..planes {
        frame.layouts.push(PlaneLayout {
            offset: r.usize()?,
            len: r.usize()?,
            stride: r.usize()?,
        });
    }
    frame.backing = match r.u8()? {
        0 => WireBacking::Memfd { len: r.usize()? },
        1 => {
            let count = usize::from(r.u8()?);
            if count != planes {
                return Err(IpcError::Malformed("bad dma-buf plane count"));
            }
            let mut spans =
                match std::mem::replace(&mut frame.backing, WireBacking::Memfd { len: 0 }) {
                    WireBacking::Dmabuf(v) => v,
                    WireBacking::Memfd { .. } => Vec::new(),
                };
            spans.clear();
            for _ in 0..count {
                spans.push((r.usize()?, r.usize()?));
            }
            WireBacking::Dmabuf(spans)
        }
        _ => return Err(IpcError::Malformed("unknown backing")),
    };
    let count = usize::from(r.u8()?);
    if count > MAX_COMPANIONS || (count > 0 && !top_level) {
        return Err(IpcError::Malformed("bad companions"));
    }
    frame.companions.truncate(count);
    for i in 0..count {
        let kind = match (r.u8()?, r.u8()?) {
            (1, level) => CompanionKind::Pyramid { level },
            (2, _) => CompanionKind::Scaled,
            (3, _) => CompanionKind::Overview,
            (4, index) => CompanionKind::Region { index },
            _ => return Err(IpcError::Malformed("unknown companion kind")),
        };
        if frame.companions.len() == i {
            frame.companions.push((kind, WireFrame::empty()));
        }
        frame.companions[i].0 = kind;
        read_frame_into(r, &mut frame.companions[i].1, false)?;
    }
    Ok(())
}

impl WireFrame {
    /// A placeholder to be filled ([`decode_frame_into`], `connection::export_into`).
    pub(super) fn empty() -> Self {
        let res = Resolution::new(1, 1).expect("non-zero");
        WireFrame {
            meta: FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), 0),
            layouts: Vec::new(),
            backing: WireBacking::Memfd { len: 0 },
            cpu_access: CpuAccess::Cached,
            companions: Vec::new(),
        }
    }
}

/// When `bytes` is a frame message: its id, the frame decoded into `frame` (its lists reused),
/// with its hops. `Ok(None)` for other messages ([`decode_server`] reads those).
pub(super) fn decode_frame_into(
    bytes: &[u8],
    frame: &mut WireFrame,
) -> Result<Option<u64>, IpcError> {
    let (mut r, kind) = Reader::start(bytes)?;
    if kind != KIND_FRAME {
        return Ok(None);
    }
    let id = r.u64()?;
    read_frame_into(&mut r, frame, true)?;
    if let Some(hops) = read_frame_hops(&mut r)? {
        frame.meta.hops = hops;
    }
    Ok(Some(id))
}

/// Bytes of a release message with its hops.
pub(super) const RELEASE_LEN: usize = 4 + 2 + 2 + 8 + 1 + 8 + 8;

/// A release message, on the stack (sent from a frame's drop: no allocation).
pub(super) fn release_bytes(id: u64, hops: ClientHops) -> [u8; RELEASE_LEN] {
    let mut out = [0u8; RELEASE_LEN];
    let fields: [&[u8]; 7] = [
        &MAGIC.to_le_bytes(),
        &VERSION.to_le_bytes(),
        &KIND_RELEASE.to_le_bytes(),
        &id.to_le_bytes(),
        &[1],
        &hops.received.unwrap_or(0).to_le_bytes(),
        &hops.imported.unwrap_or(0).to_le_bytes(),
    ];
    let mut at = 0;
    for f in fields {
        out[at..at + f.len()].copy_from_slice(f);
        at += f.len();
    }
    out
}

/// A frame message into `out` (cleared first; its room reused), with `hops` as its trailer
/// (the frame's own, with the send time).
pub(super) fn encode_frame_into(id: u64, frame: &WireFrame, hops: &FrameHops, out: &mut Vec<u8>) {
    let mut w = Writer::reuse(std::mem::take(out), KIND_FRAME);
    w.u64(id);
    write_frame(&mut w, frame);
    write_frame_hops(&mut w, hops);
    *out = w.0;
}

#[cfg(test)]
pub(super) fn encode_frame(id: u64, frame: &WireFrame) -> Vec<u8> {
    let mut out = Vec::new();
    encode_frame_into(id, frame, &frame.meta.hops, &mut out);
    out
}

#[cfg(test)]
pub(super) fn encode_release(id: u64) -> Vec<u8> {
    release_bytes(id, ClientHops::default()).to_vec()
}

pub(super) fn encode_accept(
    plan: &str,
    delivered: &Delivered,
    token: Option<ClientToken>,
) -> Vec<u8> {
    let mut w = Writer::new(KIND_ACCEPT);
    write_delivered(&mut w, delivered);
    w.text(plan);
    if let Some(t) = token {
        w.u8(TOKEN_TAG);
        w.u64(t.id);
        w.u64(t.token);
    }
    w.0
}

fn read_token(r: &mut Reader<'_>) -> Result<Option<ClientToken>, IpcError> {
    if r.0.is_empty() || r.u8()? != TOKEN_TAG {
        return Ok(None);
    }
    Ok(Some(ClientToken {
        id: r.u64()?,
        token: r.u64()?,
    }))
}

pub(super) fn encode_reject(reason: &str) -> Vec<u8> {
    let mut w = Writer::new(KIND_REJECT);
    w.text(reason);
    w.0
}

pub(super) fn encode_roi(regions: &[FrameRect]) -> Vec<u8> {
    let mut w = Writer::new(KIND_ROI);
    let regions = &regions[..regions.len().min(crate::planner::MAX_REGIONS)];
    w.u8(regions.len() as u8);
    for rect in regions {
        w.rect(*rect);
    }
    w.0
}

fn read_regions(r: &mut Reader<'_>, max: usize) -> Result<Vec<FrameRect>, IpcError> {
    let count = usize::from(r.u8()?);
    if count > max {
        return Err(IpcError::Malformed("too many regions"));
    }
    (0..count).map(|_| r.rect()).collect()
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

fn format_tag(format: MetricsFormat) -> u8 {
    match format {
        MetricsFormat::Json => 0,
        MetricsFormat::Prometheus => 1,
    }
}

fn format_from(tag: u8) -> Result<MetricsFormat, IpcError> {
    match tag {
        0 => Ok(MetricsFormat::Json),
        1 => Ok(MetricsFormat::Prometheus),
        _ => Err(IpcError::Malformed("unknown metrics format")),
    }
}

pub(super) fn encode_metrics_request(format: MetricsFormat) -> Vec<u8> {
    let mut w = Writer::new(KIND_METRICS);
    w.u8(format_tag(format));
    w.0
}

pub(super) fn encode_metrics_reply(format: MetricsFormat, len: usize) -> Vec<u8> {
    let mut w = Writer::new(KIND_METRICS_REPLY);
    w.u8(format_tag(format));
    w.usize(len);
    w.0
}

pub(super) fn decode_client(bytes: &[u8]) -> Result<ClientMessage, IpcError> {
    let (mut r, kind) = Reader::start(bytes)?;
    match kind {
        KIND_RELEASE => {
            let id = r.u64()?;
            Ok(ClientMessage::Release(id, read_release_hops(&mut r)?))
        }
        KIND_REQUEST => {
            let camera = r.opt(Reader::text)?;
            let request = read_request(&mut r)?;
            Ok(ClientMessage::Request(
                Box::new(request),
                camera,
                read_priority(&mut r)?,
            ))
        }
        KIND_LIST => Ok(ClientMessage::List),
        KIND_ROI => Ok(ClientMessage::Roi(read_regions(
            &mut r,
            crate::planner::MAX_REGIONS,
        )?)),
        KIND_METRICS => Ok(ClientMessage::Metrics(format_from(r.u8()?)?)),
        KIND_CONTROL => Ok(ClientMessage::Control(Box::new(read_control(&mut r)?))),
        _ => Err(IpcError::Malformed("unexpected message from a client")),
    }
}

pub(super) fn decode_server(bytes: &[u8]) -> Result<ServerMessage, IpcError> {
    let (mut r, kind) = Reader::start(bytes)?;
    match kind {
        KIND_FRAME => Ok(ServerMessage::Frame),
        KIND_ACCEPT => {
            let delivered = read_delivered(&mut r)?;
            let plan = r.text()?;
            Ok(ServerMessage::Accept(
                plan,
                Box::new(delivered),
                read_token(&mut r)?,
            ))
        }
        KIND_REJECT => Ok(ServerMessage::Reject(r.text()?)),
        KIND_CAMERAS => Ok(ServerMessage::Cameras(read_cameras(&mut r)?)),
        KIND_METRICS_REPLY => Ok(ServerMessage::Metrics(format_from(r.u8()?)?, r.usize()?)),
        KIND_CONTROL_REPLY => {
            let (seq, reply) = read_control_reply(&mut r)?;
            Ok(ServerMessage::ControlReply(seq, reply))
        }
        KIND_CONTROL_EVENT => Ok(ServerMessage::ControlEvent(read_control_event(&mut r)?)),
        _ => Err(IpcError::Malformed("unexpected message from a server")),
    }
}

#[cfg(test)]
mod tests;
