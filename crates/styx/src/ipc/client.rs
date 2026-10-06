//! Receiving frames in another process.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smallvec::SmallVec;
use styx_core::prelude::*;

use styx_core::metrics::HopCounters;

use super::mapcache::{CachedDmabuf, Fds, Inner, MapCache};
use super::wire::{self, CameraInfo, ClientHops, ServerMessage, WireBacking, WireFrame};
use super::{IpcError, socket};
use crate::metrics::HopMetrics;
use crate::planner::{Delivered, FrameRequest};

/// How long opening a connection may take, connecting and the service's answer together,
/// unless [`ClientOptions::timeout`] says otherwise.
pub const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Backoff between reconnection attempts.
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(2);

/// Receives frames from a [`CameraService`](super::CameraService) or a
/// [`FrameServer`](super::FrameServer) in another process.
pub struct FrameClient {
    link: Mutex<Link>,
    /// Mappings of the buffers received, kept across frames.
    maps: Arc<MapCache>,
    /// What to ask a camera service for again after reconnecting (with the latest ROI).
    request: Option<Mutex<Request>>,
    reconnect: bool,
    reconnects: AtomicU64,
    /// The receive buffers and the frame being decoded, kept between frames.
    scratch: Mutex<Scratch>,
    /// Hop times (sensor to import) and copies of the frames received.
    hops: HopCounters,
}

/// What a receive reuses: nothing is allocated per frame for the message once these have grown.
struct Scratch {
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    frame: WireFrame,
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            fds: Vec::new(),
            frame: WireFrame::empty(),
        }
    }
}

#[derive(Clone)]
struct Request {
    path: PathBuf,
    camera: Option<String>,
    frames: FrameRequest,
    /// How long each (re)connection may take.
    timeout: Duration,
}

struct Link {
    socket: Option<Arc<OwnedFd>>,
    plan: Option<String>,
    delivered: Option<Delivered>,
    next_attempt: Instant,
    backoff: Duration,
    #[cfg(feature = "async")]
    async_fd: Option<Arc<tokio::io::unix::AsyncFd<OwnedFd>>>,
}

impl Link {
    fn connected(&mut self, socket: OwnedFd, accepted: Option<(String, Delivered)>) {
        self.socket = Some(Arc::new(socket));
        (self.plan, self.delivered) = accepted.map_or((None, None), |(p, d)| (Some(p), Some(d)));
        self.backoff = RETRY_MIN;
        #[cfg(feature = "async")]
        {
            self.async_fd = None;
        }
    }

    fn lost(&mut self) {
        self.socket = None;
        #[cfg(feature = "async")]
        {
            self.async_fd = None;
        }
    }

    /// Schedule the next attempt after a failed one.
    fn failed(&mut self) {
        self.next_attempt = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(RETRY_MAX);
    }
}

/// A camera service's answer to a request: the socket, and the plan and frames it accepted.
type Opened = (OwnedFd, (String, Delivered));

/// Connect to a camera service and make `request`, within its timeout.
fn open(request: &Request) -> Result<Opened, IpcError> {
    let deadline = Instant::now() + request.timeout;
    let socket = socket::connect_until(&request.path, deadline)?;
    socket::send(
        &socket,
        &wire::encode_request(&request.frames, request.camera.as_deref()),
        &[],
    )?;
    accepted(answer(&socket, deadline)?).map(|accepted| (socket, accepted))
}

fn accepted(message: ServerMessage) -> Result<(String, Delivered), IpcError> {
    match message {
        ServerMessage::Accept(plan, delivered) => Ok((plan, *delivered)),
        ServerMessage::Reject(reason) => Err(IpcError::Rejected(reason)),
        _ => Err(IpcError::Malformed("expected an answer to the request")),
    }
}

/// [`open`] on Tokio: awaits the connection and the answer without blocking a thread; dropping
/// the future gives up.
#[cfg(feature = "async")]
async fn open_async(request: &Request) -> Result<Opened, IpcError> {
    let timed_out = || IpcError::Io(std::io::ErrorKind::TimedOut.into());
    tokio::time::timeout(request.timeout, async {
        let connecting = socket::Connecting::new(&request.path)?;
        while !connecting.attempt()? {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let socket = connecting.into_socket();
        socket::send(
            &socket,
            &wire::encode_request(&request.frames, request.camera.as_deref()),
            &[],
        )?;
        let fd = tokio::io::unix::AsyncFd::with_interest(
            socket.try_clone()?,
            tokio::io::Interest::READABLE,
        )?;
        loop {
            let mut ready = fd.readable().await?;
            match socket::recv(&socket, Duration::ZERO)? {
                socket::Received::Message(bytes, _) => match wire::decode_server(&bytes)? {
                    ServerMessage::Frame => {}
                    message => return accepted(message).map(|accepted| (socket, accepted)),
                },
                socket::Received::Nothing => ready.clear_ready(),
                socket::Received::Closed => {
                    return Err(IpcError::Io(std::io::ErrorKind::ConnectionReset.into()));
                }
            }
        }
    })
    .await
    .map_err(|_| timed_out())?
}

/// The service's answer (frames before it are skipped), waiting until `deadline`.
fn answer(socket: &OwnedFd, deadline: Instant) -> Result<ServerMessage, IpcError> {
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err(IpcError::Io(std::io::ErrorKind::TimedOut.into()));
        }
        match socket::recv(socket, wait)? {
            socket::Received::Message(bytes, _) => match wire::decode_server(&bytes)? {
                ServerMessage::Frame => {}
                message => return Ok(message),
            },
            socket::Received::Nothing => {}
            socket::Received::Closed => {
                return Err(IpcError::Io(std::io::ErrorKind::ConnectionReset.into()));
            }
        }
    }
}

/// How a [`FrameClient`] opens its connection: which camera, how long it may take, whether it
/// reconnects. From [`FrameClient::options`].
#[derive(Clone, Debug)]
pub struct ClientOptions {
    path: PathBuf,
    camera: Option<String>,
    timeout: Duration,
    reconnect: bool,
}

impl ClientOptions {
    /// From the camera this names (its name, part of it, or an identity key) rather than the
    /// service's first.
    pub fn camera(mut self, camera: impl Into<String>) -> Self {
        self.camera = Some(camera.into());
        self
    }

    /// How long opening may take, connecting and the service's answer together (default
    /// [`DEFAULT_OPEN_TIMEOUT`]); after it, opening fails with a `TimedOut` I/O error. Also the
    /// limit for each reconnection attempt.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Reconnect when the service goes away ([`FrameClient::reconnecting`]).
    pub fn reconnecting(mut self) -> Self {
        self.reconnect = true;
        self
    }

    fn request_for(&self, frames: FrameRequest) -> Request {
        Request {
            path: self.path.clone(),
            camera: self.camera.clone(),
            frames,
            timeout: self.timeout,
        }
    }

    /// Ask the camera service for the frames `request` asks for ([`FrameClient::request`]).
    pub fn request<R: Clone + Into<FrameRequest>>(
        &self,
        request: &R,
    ) -> Result<FrameClient, IpcError> {
        let request = self.request_for(request.clone().into());
        let (socket, accepted) = open(&request)?;
        Ok(FrameClient::new(
            socket,
            Some(accepted),
            Some(request),
            self.reconnect,
        ))
    }

    /// [`ClientOptions::request`] on Tokio: the connection and the service's answer are awaited
    /// without blocking a thread, and dropping the future gives up (e.g. at shutdown).
    #[cfg(feature = "async")]
    pub async fn request_async<R: Clone + Into<FrameRequest>>(
        &self,
        request: &R,
    ) -> Result<FrameClient, IpcError> {
        let request = self.request_for(request.clone().into());
        let (socket, accepted) = open_async(&request).await?;
        Ok(FrameClient::new(
            socket,
            Some(accepted),
            Some(request),
            self.reconnect,
        ))
    }

    /// The cameras the camera service serves ([`FrameClient::cameras`]).
    pub fn cameras(&self) -> Result<Vec<CameraInfo>, IpcError> {
        let deadline = Instant::now() + self.timeout;
        let socket = socket::connect_until(&self.path, deadline)?;
        socket::send(&socket, &wire::encode_list(), &[])?;
        match answer(&socket, deadline)? {
            ServerMessage::Cameras(cameras) => Ok(cameras),
            ServerMessage::Reject(reason) => Err(IpcError::Rejected(reason)),
            _ => Err(IpcError::Malformed("expected a camera list")),
        }
    }

    /// Connect to a [`FrameServer`](super::FrameServer) ([`FrameClient::connect`]).
    pub fn connect(&self) -> Result<FrameClient, IpcError> {
        let socket = socket::connect_until(&self.path, Instant::now() + self.timeout)?;
        Ok(FrameClient::new(socket, None, None, false))
    }
}

impl FrameClient {
    /// How to open a connection to the camera service or frame server at `path`: camera,
    /// timeout, reconnecting ([`ClientOptions`]).
    pub fn options(path: impl AsRef<Path>) -> ClientOptions {
        ClientOptions {
            path: path.as_ref().to_path_buf(),
            camera: None,
            timeout: DEFAULT_OPEN_TIMEOUT,
            reconnect: false,
        }
    }

    /// Connect to a [`FrameServer`](super::FrameServer).
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        Self::options(path).connect()
    }

    /// Ask the [`CameraService`](super::CameraService) at `path` for the frames `request` asks
    /// for (`&Frames::gray().size(320, 200)`), from its first camera. Fails with
    /// [`IpcError::Rejected`] (and the planner's reasons) when the camera cannot serve them next
    /// to its other clients, or a [strict](FrameRequest::strict) request would not be met;
    /// otherwise [`FrameClient::delivered`] says what the frames are. Waits up to
    /// [`DEFAULT_OPEN_TIMEOUT`]; [`FrameClient::options`] sets another.
    pub fn request<R: Clone + Into<FrameRequest>>(
        path: impl AsRef<Path>,
        request: &R,
    ) -> Result<Self, IpcError> {
        Self::options(path).request(request)
    }

    /// [`FrameClient::request`] from the camera `camera` names: its name, part of it, or one
    /// of its identity keys (see [`FrameClient::cameras`]).
    pub fn request_camera<R: Clone + Into<FrameRequest>>(
        path: impl AsRef<Path>,
        camera: &str,
        request: &R,
    ) -> Result<Self, IpcError> {
        Self::options(path).camera(camera).request(request)
    }

    /// The cameras the [`CameraService`](super::CameraService) at `path` serves.
    pub fn cameras(path: impl AsRef<Path>) -> Result<Vec<CameraInfo>, IpcError> {
        Self::options(path).cameras()
    }

    fn new(
        socket: OwnedFd,
        accepted: Option<(String, Delivered)>,
        request: Option<Request>,
        reconnect: bool,
    ) -> Self {
        let (plan, delivered) = accepted.map_or((None, None), |(p, d)| (Some(p), Some(d)));
        Self {
            link: Mutex::new(Link {
                socket: Some(Arc::new(socket)),
                plan,
                delivered,
                next_attempt: Instant::now(),
                backoff: RETRY_MIN,
                #[cfg(feature = "async")]
                async_fd: None,
            }),
            maps: Arc::new(MapCache::default()),
            reconnect: reconnect && request.is_some(),
            request: request.map(Mutex::new),
            reconnects: AtomicU64::new(0),
            scratch: Mutex::new(Scratch::default()),
            hops: HopCounters::new(),
        }
    }

    /// Hop times (the frames' path in the sending process, then their receive and import
    /// here) and copies of the frames received.
    pub fn hop_metrics(&self) -> HopMetrics {
        HopMetrics::of(&self.hops)
    }

    /// What the camera service's frames for this client are: format, size, rate, pyramid, and
    /// what of the request they do not meet (the latest, after reconnecting). `None` for a
    /// [`FrameServer`](super::FrameServer)'s frames.
    pub fn delivered(&self) -> Option<Delivered> {
        self.link.lock().delivered.clone()
    }

    /// Keep receiving across service restarts: when the connection to the camera service
    /// drops, reconnect (backing off from 100 ms to 2 s) and ask for the same frames again,
    /// with the region of interest last set. Receives return `Empty` meanwhile instead of
    /// `Closed`. Only for clients made with [`FrameClient::request`].
    pub fn reconnecting(mut self) -> Self {
        self.reconnect = self.request.is_some();
        self
    }

    /// Whether the client has a connection now.
    pub fn is_connected(&self) -> bool {
        self.link.lock().socket.is_some()
    }

    /// Times a [`FrameClient::reconnecting`] client connected again.
    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }

    /// The plan the camera service made for this client (the latest, after reconnecting).
    pub fn plan(&self) -> Option<String> {
        self.link.lock().plan.clone()
    }

    /// Change the region of interest (full-frame pixels) of a camera service's frames.
    pub fn set_roi(&self, roi: Option<FrameRect>) -> Result<(), IpcError> {
        self.set_regions(roi.as_slice())
    }

    /// Change all regions of interest of a camera service's frames at once (region 0 first;
    /// see `FrameRequest::regions`; empty: the whole frame).
    pub fn set_regions(&self, regions: &[FrameRect]) -> Result<(), IpcError> {
        if let Some(request) = &self.request {
            let mut request = request.lock();
            request.frames.roi = regions.first().copied();
            request.frames.extra_regions = regions.iter().skip(1).copied().collect();
        }
        if let Some(socket) = self.link.lock().socket.clone() {
            socket::send(&socket, &wire::encode_roi(regions), &[])?;
        }
        Ok(())
    }

    /// The next frame, waiting up to `wait`; `Closed` once the server is gone (unless the
    /// client is [reconnecting](FrameClient::reconnecting)). The server keeps the frame's
    /// buffers until the returned frame (and its companions) are dropped.
    pub fn recv(&self, wait: Duration) -> RecvOutcome<FrameLease> {
        let deadline = Instant::now() + wait;
        loop {
            let socket = self.link.lock().socket.clone();
            let Some(socket) = socket else {
                if !self.reconnect {
                    return RecvOutcome::Closed;
                }
                if self.try_reconnect() {
                    continue;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return RecvOutcome::Empty;
                }
                let next = self.link.lock().next_attempt;
                std::thread::sleep(
                    next.saturating_duration_since(Instant::now())
                        .min(remaining)
                        .max(Duration::from_millis(1)),
                );
                continue;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            // Another thread receiving at the same time gets buffers of its own.
            let mut own = None;
            let mut guard = self.scratch.try_lock();
            let scratch = match guard.as_deref_mut() {
                Some(s) => s,
                None => own.insert(Scratch::default()),
            };
            scratch.bytes.clear();
            scratch.fds.clear();
            match socket::recv_into(&socket, remaining, &mut scratch.bytes, &mut scratch.fds) {
                Ok(socket::Got::Message) => {
                    return self.frame(&socket, scratch);
                }
                Ok(socket::Got::Nothing) => return RecvOutcome::Empty,
                Ok(socket::Got::Closed) | Err(_) => {
                    drop(guard);
                    self.link.lock().lost();
                    if !self.reconnect {
                        return RecvOutcome::Closed;
                    }
                }
            }
        }
    }

    /// Connect again and repeat the request, if an attempt is due; whether it worked.
    fn try_reconnect(&self) -> bool {
        let Some(request) = &self.request else {
            return false;
        };
        if Instant::now() < self.link.lock().next_attempt {
            return false;
        }
        let request = request.lock().clone();
        match open(&request) {
            Ok((socket, accepted)) => {
                self.link.lock().connected(socket, Some(accepted));
                self.reconnects.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(err) => {
                crate::trace::debug!(error = %err, "camera service not back yet");
                self.link.lock().failed();
                false
            }
        }
    }

    /// Await the next frame; `Closed` once the server is gone (unless reconnecting).
    #[cfg(feature = "async")]
    pub async fn recv_async(&self) -> RecvOutcome<FrameLease> {
        loop {
            let connected = {
                let mut link = self.link.lock();
                match link.socket.clone() {
                    None => None,
                    Some(socket) => {
                        let fd = match &link.async_fd {
                            Some(fd) => fd.clone(),
                            None => match socket.try_clone().and_then(|dup| {
                                tokio::io::unix::AsyncFd::with_interest(
                                    dup,
                                    tokio::io::Interest::READABLE,
                                )
                            }) {
                                Ok(fd) => link.async_fd.insert(Arc::new(fd)).clone(),
                                Err(_) => return RecvOutcome::Closed,
                            },
                        };
                        Some((socket, fd))
                    }
                }
            };
            let Some((socket, fd)) = connected else {
                if !self.reconnect {
                    return RecvOutcome::Closed;
                }
                self.reconnect_async().await;
                continue;
            };
            let Ok(mut ready) = fd.readable().await else {
                return RecvOutcome::Closed;
            };
            let mut scratch = Scratch::default();
            let got = {
                let mut guard = self.scratch.try_lock();
                let s = guard.as_deref_mut().unwrap_or(&mut scratch);
                s.bytes.clear();
                s.fds.clear();
                match socket::recv_into(&socket, Duration::ZERO, &mut s.bytes, &mut s.fds) {
                    Ok(socket::Got::Message) => Some(self.frame(&socket, s)),
                    Ok(socket::Got::Nothing) => None,
                    Ok(socket::Got::Closed) | Err(_) => Some(RecvOutcome::Closed),
                }
            };
            match got {
                Some(RecvOutcome::Data(frame)) => return RecvOutcome::Data(frame),
                Some(RecvOutcome::Empty) => {}
                None => ready.clear_ready(),
                Some(RecvOutcome::Closed) => {
                    self.link.lock().lost();
                    if !self.reconnect {
                        return RecvOutcome::Closed;
                    }
                }
            }
        }
    }

    /// Wait for the next attempt, then connect again without blocking the runtime.
    #[cfg(feature = "async")]
    async fn reconnect_async(&self) {
        let Some(request) = &self.request else {
            return;
        };
        let next = self.link.lock().next_attempt;
        tokio::time::sleep_until(next.into()).await;
        let request = request.lock().clone();
        match open_async(&request).await {
            Ok((socket, accepted)) => {
                self.link.lock().connected(socket, Some(accepted));
                self.reconnects.fetch_add(1, Ordering::Relaxed);
            }
            _ => self.link.lock().failed(),
        }
    }

    /// The frame message in `scratch` imported over its descriptors: one allocation (the
    /// frame's release record) for a frame on one dma-buf.
    fn frame(&self, socket: &Arc<OwnedFd>, scratch: &mut Scratch) -> RecvOutcome<FrameLease> {
        let received = CaptureInstant::try_now().map(CaptureInstant::as_nanos);
        let imported =
            wire::decode_frame_into(&scratch.bytes, &mut scratch.frame).and_then(|id| match id {
                Some(id) => {
                    let release = Release {
                        socket: socket.clone(),
                        id,
                        hops: ClientHops {
                            received,
                            imported: None,
                        },
                    };
                    let mut fds = scratch.fds.drain(..);
                    import(&scratch.frame, &mut fds, release, &self.maps).map(Some)
                }
                None => Ok(None),
            });
        match imported {
            Ok(Some(frame)) => {
                self.hops.record(&frame.meta().hops);
                RecvOutcome::Data(frame)
            }
            Ok(None) => RecvOutcome::Empty,
            Err(err) => {
                crate::trace::warn!(error = %err, "shared frame skipped");
                RecvOutcome::Empty
            }
        }
    }
}

/// `frame` over the descriptors `fds` yields, released on the server with `release` once it
/// and its companions are dropped. Its hops get the receive (from `release`) and import times.
fn import(
    frame: &WireFrame,
    fds: &mut impl Iterator<Item = OwnedFd>,
    mut release: Release,
    maps: &Arc<MapCache>,
) -> Result<FrameLease, IpcError> {
    let imported = CaptureInstant::try_now().map(CaptureInstant::as_nanos);
    release.hops.imported = imported;
    let mut meta = frame.meta.clone();
    if let Some(ns) = release.hops.received {
        meta.hops.set(Hop::Received, ns);
    }
    if let Some(ns) = imported {
        meta.hops.set(Hop::Imported, ns);
    }
    let handle = if frame.companions.is_empty() {
        ReleaseHandle::Own(release)
    } else {
        ReleaseHandle::Shared(Arc::new(release))
    };
    let shared = match &handle {
        ReleaseHandle::Shared(r) => Some(r.clone()),
        ReleaseHandle::Own(_) => None,
    };
    let mut out = import_part(frame, meta, fds, handle, maps)?;
    for (kind, companion) in &frame.companions {
        let release = ReleaseHandle::Shared(shared.clone().expect("shared with companions"));
        let part = import_part(companion, companion.meta.clone(), fds, release, maps)?;
        out = out
            .with_companion(*kind, part)
            .map_err(|_| IpcError::Malformed("companion does not match its frame"))?;
    }
    Ok(out)
}

fn import_part(
    frame: &WireFrame,
    mut meta: FrameMeta,
    fds: &mut impl Iterator<Item = OwnedFd>,
    release: ReleaseHandle,
    maps: &Arc<MapCache>,
) -> Result<FrameLease, IpcError> {
    let count = frame.backing.fd_count();
    let own: Fds = fds.by_ref().take(count).collect();
    if own.len() != count {
        return Err(IpcError::Malformed("descriptors missing"));
    }
    let layouts: SmallVec<[PlaneLayout; 3]> = frame.layouts.iter().copied().collect();
    let inner = match &frame.backing {
        WireBacking::Dmabuf(spans) => {
            if spans.len() != layouts.len() {
                return Err(FrameExportError::PlaneCountMismatch {
                    expected: layouts.len(),
                    actual: spans.len(),
                }
                .into());
            }
            meta.residency = Some(FrameResidency::Dmabuf);
            // Planes on one buffer (the usual case) read through the cached mappings.
            match CachedDmabuf::new(maps, own, spans) {
                Ok(cached) => Inner::Cached(cached),
                Err(own) => {
                    let planes = own
                        .into_iter()
                        .zip(spans)
                        .map(|(fd, &(offset, len))| FrameFdPlane { fd, offset, len })
                        .collect();
                    let f = FrameLease::from_dmabuf(meta.clone(), layouts.clone(), planes)?;
                    Inner::Other(external(&f)?)
                }
            }
        }
        WireBacking::Memfd { len } => {
            let fd = own.into_iter().next().expect("one descriptor");
            // Read through the cached mappings too (a capture cycles through its memfds).
            match CachedDmabuf::memfd(maps, fd, *len, layouts.len()) {
                Ok(cached) => {
                    meta.residency = Some(FrameResidency::HostExternal);
                    Inner::Cached(cached)
                }
                Err(fd) => {
                    let f = FrameLease::from_memfd(meta.clone(), layouts.clone(), fd);
                    meta.residency = f.meta().residency;
                    Inner::Other(external(&f)?)
                }
            }
        }
    };
    Ok(FrameLease::from_external(
        meta,
        layouts,
        Arc::new(Released {
            inner,
            _release: release,
            cpu_access: frame.cpu_access,
        }),
    ))
}

fn external(frame: &FrameLease) -> Result<Arc<dyn ExternalBacking>, IpcError> {
    frame
        .external_backing_handle()
        .ok_or(IpcError::Malformed("frame without backing"))
}

/// Tells the server a frame was dropped, so it can let go of its buffers; with the frame's
/// receive and import times here.
struct Release {
    socket: Arc<OwnedFd>,
    id: u64,
    hops: ClientHops,
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = socket::send(&self.socket, &wire::release_bytes(self.id, self.hops), &[]);
    }
}

/// A frame's release: its own (no companions), or shared with its companions.
enum ReleaseHandle {
    Own(#[allow(dead_code)] Release),
    Shared(#[allow(dead_code)] Arc<Release>),
}

/// A received frame's memory; the frame is released on the server once it and its companions
/// are all dropped.
struct Released {
    inner: Inner,
    _release: ReleaseHandle,
    /// As the sender reported it for its memory.
    cpu_access: CpuAccess,
}

impl ExternalBacking for Released {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.inner.backing().plane_data(index)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.inner.backing().backing_bytes()
    }

    fn backing_kind(&self) -> &'static str {
        "ipc"
    }

    fn can_export(&self) -> bool {
        self.inner.backing().can_export()
    }

    fn cpu_access(&self) -> CpuAccess {
        self.cpu_access
    }

    fn residency(&self) -> FrameResidency {
        self.inner.backing().residency()
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
