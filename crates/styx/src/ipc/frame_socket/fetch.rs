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
/// ([`FrameFetcher::hop_metrics`]), each frame once: a frame fetched again (same timestamp) is
/// counted in [`FetchStats::repeated`] instead.
///
/// [`FrameFetcher::fetch`] gets the latest frame, whichever it is; [`FrameFetcher::fetch_next`]
/// waits for a frame this fetcher has not had yet, so a consumer reading every frame calls it
/// in a loop without polling.
pub struct FrameFetcher {
    path: PathBuf,
    next_path: PathBuf,
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    maps: Arc<MapCache>,
    hops: HopCounters,
    /// Timestamp of the last frame fetched (`0`: none yet).
    last: u64,
    /// The last frame fetched was the one fetched before it.
    repeat: bool,
    stats: FetchStats,
}

/// What a [`FrameFetcher`] fetched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// Frames received (every fetch that returned one).
    pub fetched: u64,
    /// Of those, the same frame as the fetch before (the server had published nothing new).
    pub repeated: u64,
    /// `fetch_next` calls answered on the frame socket itself because the server has no
    /// `<path>.next` endpoint (an older Styx): each polled until a new frame came.
    pub polled: u64,
}

impl FrameFetcher {
    /// Fetches from the frame socket at `path`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            next_path: super::next_path(path),
            bytes: Vec::new(),
            fds: Vec::new(),
            maps: Arc::new(MapCache::default()),
            hops: HopCounters::new(),
            last: 0,
            repeat: false,
            stats: FetchStats::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The latest frame (see [`fetch_frame`]); its hops are recorded here unless it is the frame
    /// fetched before.
    pub fn fetch(&mut self, wait: Duration) -> Result<FrameLease, IpcError> {
        let socket = socket::connect_stream(&self.path)?;
        self.receive(socket, Instant::now() + wait)
    }

    /// The next frame this fetcher has not had (the first: the latest), waiting up to `wait`
    /// for the server to publish it: from the frame socket's `<path>.next` endpoint
    /// ([`next_path`](super::next_path)), which sends each frame once, when it is published.
    /// Against a server without that endpoint it fetches from the frame socket until the
    /// frame is a new one, a few milliseconds apart ([`FetchStats::polled`]).
    pub fn fetch_next(&mut self, wait: Duration) -> Result<FrameLease, IpcError> {
        let deadline = Instant::now() + wait;
        match socket::connect_stream(&self.next_path) {
            Ok(socket) => {
                let mut line = [0u8; 24];
                let line = super::next::request(self.last, &mut line);
                let stream = std::os::unix::net::UnixStream::from(socket);
                // A fresh connection's buffer takes a line without blocking.
                std::io::Write::write_all(&mut &stream, line)?;
                self.receive(OwnedFd::from(stream), deadline)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                self.stats.polled += 1;
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let frame = self.fetch(left)?;
                    if !self.repeat {
                        return Ok(frame);
                    }
                    drop(frame);
                    if left.is_zero() {
                        return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
                    }
                    std::thread::sleep(left.min(Duration::from_millis(2)));
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// What this fetcher fetched.
    pub fn stats(&self) -> FetchStats {
        self.stats
    }

    /// Receives the frame message on `socket` (connected), up to `deadline`.
    fn receive(&mut self, socket: OwnedFd, deadline: Instant) -> Result<FrameLease, IpcError> {
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
        let timestamp = frame.meta().timestamp;
        let repeated = self.stats.fetched > 0 && timestamp == self.last;
        self.stats.fetched += 1;
        if repeated {
            self.stats.repeated += 1;
            self.repeat = true;
        } else {
            self.repeat = false;
            self.hops.record(&frame.meta().hops);
        }
        self.last = timestamp;
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
