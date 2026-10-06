//! The consumer side of a frame socket: fetching the latest frame, leased, with its hops.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smallvec::SmallVec;
use styx_core::lease_codec::{self, LeaseBackingInline, LeaseMessageInline};
use styx_core::metrics::HopCounters;
use styx_core::prelude::*;

use super::super::mapcache::{CachedDmabuf, Fds, Inner, MapCache};
use super::super::{IpcError, socket};
use crate::metrics::HopMetrics;

/// Fetch the latest frame from a [`FrameSocket`](super::FrameSocket) (or any
/// `styx-frame-lease-v1` server), waiting up to `wait` for it. The frame is leased: the
/// connection stays open, and the server keeps the frame's buffer, until the returned lease (and
/// every view of it) is dropped. Its hops (`FrameMeta::hops`) end with this process's receive
/// and import times. A consumer fetching again and again uses a [`FrameFetcher`], which keeps
/// its buffers and the mappings of the frames' buffers between fetches.
pub fn fetch_frame(path: impl AsRef<Path>, wait: Duration) -> Result<FrameLease, IpcError> {
    FrameFetcher::new(path).fetch(wait)
}

/// Fetches frames from one frame socket again and again. Its receive buffers are kept between
/// fetches, and so is the mapping of each buffer it has read (a capture cycles through a few
/// buffers, sent as new descriptors each time: mapping one costs more than reading it), so a
/// fetch allocates one record (the frame's lease) and maps nothing in steady state. The hop
/// times of the frames it fetched (sensor to import, with the server's hops) are recorded
/// ([`FrameFetcher::hop_metrics`]).
pub struct FrameFetcher {
    path: PathBuf,
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    maps: Arc<MapCache>,
    hops: HopCounters,
}

impl FrameFetcher {
    /// Fetches from the frame socket at `path`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            bytes: Vec::new(),
            fds: Vec::new(),
            maps: Arc::new(MapCache::default()),
            hops: HopCounters::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The latest frame (see [`fetch_frame`]); its hops are recorded here.
    pub fn fetch(&mut self, wait: Duration) -> Result<FrameLease, IpcError> {
        let socket = socket::connect_stream(&self.path)?;
        let deadline = Instant::now() + wait;
        self.bytes.clear();
        self.fds.clear();
        const HEADER: usize = lease_codec::HEADER_LEN;
        let payload_len = loop {
            if self.bytes.len() >= HEADER {
                let header = self.bytes[..HEADER].try_into().expect("header");
                let len = lease_codec::payload_len(header)?;
                if self.bytes.len() >= HEADER + len {
                    break len;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match socket::recv_into(&socket, left, &mut self.bytes, &mut self.fds)? {
                socket::Got::Message => {}
                socket::Got::Nothing if left.is_zero() => {
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
                }
                socket::Got::Nothing => {}
                socket::Got::Closed => return Err(IpcError::Malformed("no frame served")),
            }
        };
        let received = CaptureInstant::try_now();
        let message = LeaseMessageInline::parse(&self.bytes[HEADER..HEADER + payload_len])?;
        message.check(&self.fds)?;
        let mut meta = message.meta().ok_or(FrameExportError::InvalidDescriptor)?;
        let layouts = message.descriptor.layouts();
        let inner = self.import(&message, &mut meta)?;
        if let Some(at) = received {
            meta.hops.set(Hop::Received, at.as_nanos());
        }
        meta.hops.mark(Hop::Imported);
        let frame = FrameLease::from_external(
            meta,
            layouts,
            Arc::new(Fetched {
                inner,
                _connection: socket,
            }),
        );
        match frame.validate_plane_layouts() {
            Ok(()) | Err(FrameValidationError::UnknownStorageLayout) => {}
            Err(_) => return Err(FrameExportError::InvalidDescriptor.into()),
        }
        self.hops.record(&frame.meta().hops);
        Ok(frame)
    }

    /// The frame's memory over the descriptors received (checked), through the mapping cache
    /// when they are one buffer.
    fn import(
        &mut self,
        message: &LeaseMessageInline,
        meta: &mut FrameMeta,
    ) -> Result<Inner, IpcError> {
        let external = |frame: FrameLease| {
            frame
                .external_backing_handle()
                .ok_or(IpcError::Malformed("frame without backing"))
        };
        Ok(match &message.backing {
            LeaseBackingInline::Memfd { len } => {
                let fd = self.fds.pop().expect("checked: one descriptor");
                let planes = message.descriptor.planes.len();
                meta.residency = Some(FrameResidency::HostExternal);
                match CachedDmabuf::memfd(&self.maps, fd, *len, planes) {
                    Ok(cached) => Inner::Cached(cached),
                    Err(fd) => Inner::Other(external(FrameLease::from_memfd_import(
                        message.descriptor.clone(),
                        fd,
                    )?)?),
                }
            }
            LeaseBackingInline::DmabufPlanes { planes } => {
                let fds: Fds = self.fds.drain(..).collect();
                let spans: SmallVec<[(usize, usize); 4]> =
                    planes.iter().map(|p| (p.offset, p.len)).collect();
                meta.residency = Some(FrameResidency::Dmabuf);
                match CachedDmabuf::new(&self.maps, fds, &spans) {
                    Ok(cached) => Inner::Cached(cached),
                    Err(fds) => {
                        let planes = fds
                            .into_iter()
                            .zip(planes)
                            .map(|(fd, p)| FrameFdPlane {
                                fd,
                                offset: p.offset,
                                len: p.len,
                            })
                            .collect();
                        Inner::Other(external(FrameLease::from_dmabuf_import(
                            message.descriptor.clone(),
                            planes,
                        )?)?)
                    }
                }
            }
        })
    }

    /// Hop times (the server's, then receive and import here) and copies of the frames fetched.
    pub fn hop_metrics(&self) -> HopMetrics {
        HopMetrics::of(&self.hops)
    }
}

/// Import `payload` as a frame message over `fds` (memfds standing in for what a server
/// sends) with [`lease_codec::decode`] and read every plane, as a consumer would; and parse and
/// check it as [`FrameFetcher`] does. For fuzzing.
#[doc(hidden)]
pub fn fuzz_import(payload: &[u8], fds: Vec<OwnedFd>) {
    if let Ok(message) = LeaseMessageInline::parse(payload) {
        let _ = message.check(&fds);
        let _ = std::hint::black_box(message.meta());
    }
    if let Ok(frame) = lease_codec::decode(payload, fds) {
        let _ = frame.validate_plane_layouts();
        let _ = std::hint::black_box(frame.meta().hop_record());
        if let Ok(planes) = frame.planes_visible() {
            for rows in planes {
                for row in rows {
                    std::hint::black_box(row.data().iter().fold(0u8, |a, b| a ^ b));
                }
            }
        }
        let _ = std::hint::black_box(frame.to_visible_vec());
    }
}

/// A fetched frame's memory and the connection that is its lease.
struct Fetched {
    inner: Inner,
    _connection: OwnedFd,
}

impl ExternalBacking for Fetched {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.inner.backing().plane_data(index)
    }

    fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        self.inner.backing().begin_cpu_read(index)
    }

    fn end_cpu_read(&self, index: usize) {
        self.inner.backing().end_cpu_read(index)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.inner.backing().backing_bytes()
    }

    fn backing_kind(&self) -> &'static str {
        "frame_socket"
    }

    fn can_export(&self) -> bool {
        self.inner.backing().can_export()
    }

    fn residency(&self) -> FrameResidency {
        self.inner.backing().residency()
    }

    fn dmabuf_plane(&self, index: usize) -> Option<DmabufPlane<'_>> {
        self.inner.backing().dmabuf_plane(index)
    }

    fn cpu_access(&self) -> CpuAccess {
        self.inner.backing().cpu_access()
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        self.inner.backing().export_backing()
    }

    fn export_into(
        &self,
        out: &mut Vec<FrameFdPlane>,
    ) -> Result<Option<ExportedKind>, FrameExportError> {
        self.inner.export_into(out)
    }
}
