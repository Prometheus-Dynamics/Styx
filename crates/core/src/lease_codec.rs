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
//! An endpoint record names a frame socket as `styx-frame-lease+unix://<absolute path>`
//! ([`endpoint_uri`], [`parse_endpoint_uri`]): scheme [`ENDPOINT_SCHEME`], the socket path as
//! its payload.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::buffer::{
    FrameBackingExport, FrameExportError, FrameFdPlane, FrameLease, FrameLeaseDescriptor, fd_size,
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
    let (descriptor, backing) = frame.export_or_copy_memfd()?;
    let (backing, fds) = match backing {
        FrameBackingExport::Memfd { fd, len } => (LeaseBacking::Memfd { len }, vec![fd]),
        FrameBackingExport::DmabufPlanes { planes } => {
            let wire = planes
                .iter()
                .map(|p| LeasePlane {
                    offset: p.offset,
                    len: p.len,
                })
                .collect();
            let fds = planes.into_iter().map(|p| p.fd).collect();
            (LeaseBacking::DmabufPlanes { planes: wire }, fds)
        }
    };
    if fds.len() > MAX_FDS {
        return Err(LeaseCodecError::TooManyFds(fds.len()));
    }
    let payload = LeaseMessage {
        descriptor,
        backing,
    }
    .to_json()?;
    Ok((payload, fds))
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
    Ok(match message.backing {
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
    })
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
