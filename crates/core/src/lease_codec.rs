//! The `styx-frame-lease-v1` frame message, without a transport: what Styx's frame socket
//! (`styx::ipc::FrameSocket`) sends for a frame, and what a consumer imports from it.
//!
//! A message is a JSON payload ([`LeaseMessage`]) and the file descriptors behind the frame,
//! passed together (on a Unix socket: one `sendmsg` with `SCM_RIGHTS`):
//!
//! ```json
//! {"descriptor":{"width":64,"height":32,"fourcc":"NV12","timestamp":7,"color":"Bt709",
//!                "planes":[{"offset":0,"len":2048,"stride":64},{"offset":2048,"len":1024,"stride":64}]},
//!  "backing":{"kind":"memfd","len":3072}}
//! ```
//!
//! **Descriptor order.** [`LeaseBacking::Memfd`]: exactly one descriptor, every plane at its
//! `descriptor.planes[i].offset` in it. [`LeaseBacking::DmabufPlanes`]: one descriptor per
//! entry of `planes`, in that order; plane `i` of the descriptor lies at its offset within the
//! `len` bytes starting at `planes[i].offset` of descriptor `i` (several entries may be
//! duplicates of one dma-buf). At most [`MAX_FDS`] descriptors and [`MAX_PAYLOAD`] bytes of JSON.
//!
//! The frame socket frames a message on its `SOCK_STREAM` connection as a little-endian `u32`
//! payload length ([`HEADER_LEN`] bytes), then the payload, the descriptors attached to the
//! same `sendmsg` ([`encode_framed`], [`payload_len`]). Leases and flow control are the
//! transport's (the frame socket's connection is the lease); this module only encodes and
//! checks messages, so other transports (or a test) can carry them.
//!
//! **Hops.** A message may carry the frame's hop record (`"hops"`, a [`HopRecord`]: the
//! sequence number and `CLOCK_MONOTONIC` nanoseconds of each step from the sensor to the send,
//! and the copies made on the way), the last member of the object:
//!
//! ```json
//! {"descriptor":{...},"backing":{...},
//!  "hops":{"sequence":7,"sensor":1000,"dequeued":9000,"isp_done":11000,"queued":11200,
//!          "taken":11300,"sent":11400}}
//! ```
//!
//! It is optional both ways: [`encode`] leaves it out for a frame without hops (the bytes are
//! then those of a peer that predates it), and a peer that does not know it ignores it (serde
//! skips unknown members). [`decode`] puts it in the frame's [`FrameMeta::hops`](crate::buffer::FrameMeta::hops),
//! the sequence number included ([`FrameMeta::sequence`](crate::buffer::FrameMeta::sequence)).
//!
//! An endpoint record names a frame socket as `styx-frame-lease+unix://<absolute path>`
//! ([`endpoint_uri`], [`parse_endpoint_uri`]): scheme [`ENDPOINT_SCHEME`], the socket path as
//! its payload.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::buffer::{
    FrameBackingExport, FrameExportError, FrameFdPlane, FrameLease, FrameLeaseDescriptor,
    HopRecord, fd_size,
};

/// The transport name a provider advertises for a frame socket.
pub const TRANSPORT: &str = "styx-frame-lease-v1";
/// Most descriptors one message carries.
pub const MAX_FDS: usize = 4;
/// Largest JSON payload, in bytes.
pub const MAX_PAYLOAD: usize = 64 * 1024;
/// Bytes of the length that precedes a payload on a frame socket (`u32`, little-endian).
pub const HEADER_LEN: usize = 4;
/// The endpoint record scheme of a frame socket (`styx-frame-lease+unix://<path>`).
pub const ENDPOINT_SCHEME: &str = "styx-frame-lease+unix";

/// A frame message's JSON: what the frame is and how its descriptors hold it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseMessage {
    pub descriptor: FrameLeaseDescriptor,
    pub backing: LeaseBacking,
    /// The frame's hops up to the send (see the module documentation); `None` for a frame
    /// without hops, or from a peer that does not send them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hops: Option<HopRecord>,
}

/// How the descriptors sent with a [`LeaseMessage`] hold the frame (JSON: `"kind"` is
/// `"memfd"` or `"dmabuf_planes"`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseBacking {
    /// One descriptor (a memfd, or any mappable file) of `len` bytes holding every plane.
    Memfd { len: usize },
    /// One descriptor per entry, in order: the bytes of plane `i` lie in descriptor `i`.
    DmabufPlanes { planes: Vec<LeasePlane> },
}

/// Where a plane's bytes are within its descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeasePlane {
    pub offset: usize,
    pub len: usize,
}

/// Why a frame message could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum LeaseCodecError {
    #[error("frame message payload of {0} bytes is larger than {MAX_PAYLOAD}")]
    PayloadTooLarge(usize),
    #[error("frame message carries {0} descriptors, more than {MAX_FDS}")]
    TooManyFds(usize),
    #[error("frame message is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{backing} backing needs {expected} descriptors, the message carries {actual}")]
    FdCount {
        backing: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error(
        "memfd backing of {len} bytes does not hold the planes or is larger than its descriptor"
    )]
    MemfdSize { len: usize },
    /// The frame could not be exported, or the descriptor does not match the memory sent
    /// (planes outside their descriptors, a plane count or layout that does not fit).
    #[error(transparent)]
    Frame(#[from] FrameExportError),
}

impl LeaseMessage {
    /// Parse a payload (without its length header), checking its size; the descriptors are
    /// checked by [`decode`].
    pub fn parse(payload: &[u8]) -> Result<Self, LeaseCodecError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(LeaseCodecError::PayloadTooLarge(payload.len()));
        }
        Ok(serde_json::from_slice(payload)?)
    }

    /// The JSON payload.
    pub fn to_json(&self) -> Result<Vec<u8>, LeaseCodecError> {
        let payload = serde_json::to_vec(self)?;
        if payload.len() > MAX_PAYLOAD {
            return Err(LeaseCodecError::PayloadTooLarge(payload.len()));
        }
        Ok(payload)
    }

    /// Descriptors the message needs.
    pub fn fd_count(&self) -> usize {
        match &self.backing {
            LeaseBacking::Memfd { .. } => 1,
            LeaseBacking::DmabufPlanes { planes } => planes.len(),
        }
    }
}

/// `frame` as a message: its JSON payload and the descriptors to send with it, in order.
/// Dma-bufs and memfds are passed as they are (duplicated descriptors); other memory is copied
/// once into a new memfd. Nothing keeps the frame's buffer: a sender that must not see it
/// rewritten keeps `frame` (or its backing) for as long as the receiver may read it.
#[cfg(target_os = "linux")]
pub fn encode(frame: &FrameLease) -> Result<(Vec<u8>, Vec<OwnedFd>), LeaseCodecError> {
    let (message, fds) = encode_message(frame)?;
    Ok((message.to_json()?, fds))
}

/// [`encode`] before the JSON: the message (its [`hops`](LeaseMessage::hops) from the frame's
/// metadata, with a copy into a memfd counted) and the descriptors, for a transport that adds
/// to the hops before it sends (e.g. the send time).
#[cfg(target_os = "linux")]
pub fn encode_message(frame: &FrameLease) -> Result<(LeaseMessage, Vec<OwnedFd>), LeaseCodecError> {
    let mut encoded = EncodedFrame::default();
    encoded.fill(frame)?;
    let message = LeaseMessage {
        descriptor: encoded.descriptor.clone(),
        backing: match encoded.memfd_len {
            Some(len) => LeaseBacking::Memfd { len },
            None => LeaseBacking::DmabufPlanes {
                planes: std::mem::take(&mut encoded.planes),
            },
        },
        hops: encoded.hops,
    };
    Ok((message, std::mem::take(&mut encoded.fds)))
}

/// A frame encoded for sending, kept to send any number of times and refilled for the next
/// frame without allocating once its lists have grown ([`EncodedFrame::fill`]).
#[derive(Debug)]
pub struct EncodedFrame {
    pub descriptor: FrameLeaseDescriptor,
    /// `Some(len)`: one memfd of `len` bytes holds every plane; `None`: one descriptor per
    /// entry of `planes`.
    pub memfd_len: Option<usize>,
    pub planes: Vec<LeasePlane>,
    /// The descriptors, in message order.
    pub fds: Vec<OwnedFd>,
    /// The frame's hops (with a memfd copy counted); `None` for a frame without hops.
    pub hops: Option<HopRecord>,
    /// The frame was copied into a memfd (it was not in shareable memory).
    pub copied: bool,
}

impl Default for EncodedFrame {
    fn default() -> Self {
        Self {
            descriptor: FrameLeaseDescriptor {
                width: 0,
                height: 0,
                fourcc: crate::format::FourCc::GREY,
                timestamp: 0,
                color: crate::format::ColorSpace::Unknown,
                planes: smallvec::SmallVec::new(),
            },
            memfd_len: None,
            planes: Vec::new(),
            fds: Vec::new(),
            hops: None,
            copied: false,
        }
    }
}

impl EncodedFrame {
    /// `frame`, replacing what was there: its descriptor and descriptors (dma-bufs and memfds
    /// duplicated, other memory copied once into a new memfd) and its hops.
    #[cfg(target_os = "linux")]
    pub fn fill(&mut self, frame: &FrameLease) -> Result<(), LeaseCodecError> {
        self.fds.clear();
        self.planes.clear();
        let exportable = frame
            .external_backing_handle()
            .is_some_and(|b| b.can_export());
        let (descriptor, backing) = frame.export_or_copy_memfd()?;
        self.descriptor = descriptor;
        match backing {
            FrameBackingExport::Memfd { fd, len } => {
                self.memfd_len = Some(len);
                self.fds.push(fd);
            }
            FrameBackingExport::DmabufPlanes { planes } => {
                self.memfd_len = None;
                for p in planes {
                    self.planes.push(LeasePlane {
                        offset: p.offset,
                        len: p.len,
                    });
                    self.fds.push(p.fd);
                }
            }
        }
        if self.fds.len() > MAX_FDS {
            let n = self.fds.len();
            self.fds.clear();
            return Err(LeaseCodecError::TooManyFds(n));
        }
        self.copied = !exportable;
        let mut hops = frame.meta().hops;
        if self.copied {
            hops.copied(frame.layout_slice().iter().map(|l| l.len).sum());
        }
        let record = HopRecord::new(frame.meta().sequence(), &hops);
        self.hops = (!record.is_empty()).then_some(record);
        Ok(())
    }

    /// Lets go of the descriptors (the frame's memory), keeping the lists' room.
    pub fn clear(&mut self) {
        self.fds.clear();
        self.planes.clear();
        self.hops = None;
    }

    /// The message, with `hops` (e.g. [`EncodedFrame::hops`] with the send time added).
    pub fn message<'a>(&'a self, hops: Option<&'a HopRecord>) -> LeaseMessageRef<'a> {
        LeaseMessageRef {
            descriptor: &self.descriptor,
            backing: match self.memfd_len {
                Some(len) => LeaseBackingRef::Memfd { len },
                None => LeaseBackingRef::DmabufPlanes {
                    planes: &self.planes,
                },
            },
            hops,
        }
    }
}

/// [`encode`], the payload preceded by its length as the frame socket sends it.
#[cfg(target_os = "linux")]
pub fn encode_framed(frame: &FrameLease) -> Result<(Vec<u8>, Vec<OwnedFd>), LeaseCodecError> {
    let (payload, fds) = encode(frame)?;
    let mut message = Vec::with_capacity(HEADER_LEN + payload.len());
    message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    message.extend_from_slice(&payload);
    Ok((message, fds))
}

/// The payload length a frame socket's header announces, refused beyond [`MAX_PAYLOAD`].
pub fn payload_len(header: [u8; HEADER_LEN]) -> Result<usize, LeaseCodecError> {
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_PAYLOAD {
        return Err(LeaseCodecError::PayloadTooLarge(len));
    }
    Ok(len)
}

/// The frame a payload (without its length header) describes, over the descriptors that came
/// with it. Checks the payload size, the descriptor count against the backing, that every
/// plane lies within its descriptor (and a memfd backing within its memfd), and that the plane
/// layouts hold the format. The frame maps the descriptors on first read.
pub fn decode(payload: &[u8], fds: Vec<OwnedFd>) -> Result<FrameLease, LeaseCodecError> {
    if fds.len() > MAX_FDS {
        return Err(LeaseCodecError::TooManyFds(fds.len()));
    }
    let message = LeaseMessage::parse(payload)?;
    decode_message(message, fds)
}

/// A [`LeaseMessage`] borrowed from its parts, for a sender that keeps them between sends
/// (written with [`LeaseMessageRef::write_framed`], the same bytes as the owned message).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct LeaseMessageRef<'a> {
    pub descriptor: &'a FrameLeaseDescriptor,
    pub backing: LeaseBackingRef<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hops: Option<&'a HopRecord>,
}

/// [`LeaseBacking`], borrowed.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseBackingRef<'a> {
    Memfd { len: usize },
    DmabufPlanes { planes: &'a [LeasePlane] },
}

impl LeaseMessage {
    /// The message, borrowed.
    pub fn as_ref(&self) -> LeaseMessageRef<'_> {
        LeaseMessageRef {
            descriptor: &self.descriptor,
            backing: match &self.backing {
                LeaseBacking::Memfd { len } => LeaseBackingRef::Memfd { len: *len },
                LeaseBacking::DmabufPlanes { planes } => LeaseBackingRef::DmabufPlanes { planes },
            },
            hops: self.hops.as_ref(),
        }
    }
}

impl LeaseMessageRef<'_> {
    /// Descriptors the message needs.
    pub fn fd_count(&self) -> usize {
        match self.backing {
            LeaseBackingRef::Memfd { .. } => 1,
            LeaseBackingRef::DmabufPlanes { planes } => planes.len(),
        }
    }

    /// [`write_framed`] for the borrowed message.
    pub fn write_framed(&self, out: &mut Vec<u8>) -> Result<(), LeaseCodecError> {
        out.clear();
        out.extend_from_slice(&[0; HEADER_LEN]);
        serde_json::to_writer(&mut *out, self)?;
        finish_framed(out)
    }
}

/// Writes `message` as a frame socket sends it into `out` (cleared first): the length header
/// and the JSON payload. Reusing `out` across messages, nothing is allocated once it has grown
/// to a message's size.
pub fn write_framed(message: &LeaseMessage, out: &mut Vec<u8>) -> Result<(), LeaseCodecError> {
    message.as_ref().write_framed(out)
}

fn finish_framed(out: &mut [u8]) -> Result<(), LeaseCodecError> {
    let len = out.len() - HEADER_LEN;
    if len > MAX_PAYLOAD {
        return Err(LeaseCodecError::PayloadTooLarge(len));
    }
    out[..HEADER_LEN].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(())
}

/// [`decode`] for an already parsed message.
pub fn decode_message(
    message: LeaseMessage,
    mut fds: Vec<OwnedFd>,
) -> Result<FrameLease, LeaseCodecError> {
    let expected = message.fd_count();
    if fds.len() != expected {
        let backing = match message.backing {
            LeaseBacking::Memfd { .. } => "memfd",
            LeaseBacking::DmabufPlanes { .. } => "dmabuf_planes",
        };
        return Err(LeaseCodecError::FdCount {
            backing,
            expected,
            actual: fds.len(),
        });
    }
    if expected > MAX_FDS {
        return Err(LeaseCodecError::TooManyFds(expected));
    }
    let hops = message.hops;
    let mut frame = match message.backing {
        LeaseBacking::Memfd { len } => {
            let fd = fds.pop().expect("one descriptor");
            let within_len = message
                .descriptor
                .planes
                .iter()
                .all(|p| p.offset.checked_add(p.len).is_some_and(|end| end <= len));
            let within_fd = fd_size(&fd).is_none_or(|size| len as u64 <= size);
            if !within_len || !within_fd {
                return Err(LeaseCodecError::MemfdSize { len });
            }
            FrameLease::from_memfd_import(message.descriptor, fd)?
        }
        LeaseBacking::DmabufPlanes { planes } => {
            let planes = fds
                .into_iter()
                .zip(planes)
                .map(|(fd, p)| FrameFdPlane {
                    fd,
                    offset: p.offset,
                    len: p.len,
                })
                .collect();
            FrameLease::from_dmabuf_import(message.descriptor, planes)?
        }
    };
    if let Some(record) = hops {
        frame.meta_mut().hops = record.hops();
    }
    Ok(frame)
}

/// The endpoint record URI of a frame socket at `path`: `styx-frame-lease+unix://` and the
/// path (so `styx-frame-lease+unix:///run/styx/front.sock`). `None` for a relative or
/// non-UTF-8 path. An Orion `ResourceEndpoint::Custom` carries the scheme
/// ([`ENDPOINT_SCHEME`]) and the path (the payload) apart; [`endpoint_payload`] gives the latter.
pub fn endpoint_uri(path: &Path) -> Option<String> {
    Some(format!("{ENDPOINT_SCHEME}://{}", endpoint_payload(path)?))
}

/// The payload of a frame socket's endpoint record: its absolute path, as UTF-8.
pub fn endpoint_payload(path: &Path) -> Option<&str> {
    path.is_absolute().then_some(())?;
    path.to_str()
}

/// The socket path of a `styx-frame-lease+unix://<path>` URI (the scheme in any case); `None`
/// for another scheme or a path that is not absolute.
pub fn parse_endpoint_uri(uri: &str) -> Option<PathBuf> {
    let (scheme, rest) = uri.split_once("://")?;
    parse_endpoint(scheme, rest)
}

/// The socket path of an endpoint record given as scheme and payload (the scheme in any case,
/// the payload an absolute path).
pub fn parse_endpoint(scheme: &str, payload: &str) -> Option<PathBuf> {
    if !scheme.eq_ignore_ascii_case(ENDPOINT_SCHEME) {
        return None;
    }
    let path = Path::new(payload);
    path.is_absolute().then(|| path.to_path_buf())
}

#[cfg(test)]
#[path = "lease_codec_tests.rs"]
mod tests;
