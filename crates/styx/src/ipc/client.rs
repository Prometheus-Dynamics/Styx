//! Receiving frames in another process.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smallvec::SmallVec;
use styx_core::prelude::*;

use super::mapcache::{CachedDmabuf, MapCache};
use super::wire::{self, CameraInfo, ServerMessage, WireBacking, WireFrame};
use super::{IpcError, socket};
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
                    ServerMessage::Frame(..) => {}
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
                ServerMessage::Frame(..) => {}
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
        }
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
        if let Some(request) = &self.request {
            request.lock().frames.roi = roi;
        }
        if let Some(socket) = self.link.lock().socket.clone() {
            socket::send(&socket, &wire::encode_roi(roi), &[])?;
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
            match socket::recv(&socket, remaining) {
                Ok(socket::Received::Message(bytes, fds)) => {
                    return self.frame(&socket, &bytes, fds);
                }
                Ok(socket::Received::Nothing) => return RecvOutcome::Empty,
                Ok(socket::Received::Closed) | Err(_) => {
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
                tracing::debug!(error = %err, "camera service not back yet");
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
            match socket::recv(&socket, Duration::ZERO) {
                Ok(socket::Received::Message(bytes, fds)) => {
                    if let RecvOutcome::Data(frame) = self.frame(&socket, &bytes, fds) {
                        return RecvOutcome::Data(frame);
                    }
                }
                Ok(socket::Received::Nothing) => ready.clear_ready(),
                Ok(socket::Received::Closed) | Err(_) => {
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

    fn frame(
        &self,
        socket: &Arc<OwnedFd>,
        bytes: &[u8],
        fds: Vec<OwnedFd>,
    ) -> RecvOutcome<FrameLease> {
        let imported = wire::decode_server(bytes).and_then(|message| match message {
            ServerMessage::Frame(id, frame) => {
                let release = Arc::new(Release {
                    socket: socket.clone(),
                    id,
                });
                import(*frame, &mut fds.into_iter(), &release, &self.maps).map(Some)
            }
            _ => Ok(None),
        });
        match imported {
            Ok(Some(frame)) => RecvOutcome::Data(frame),
            Ok(None) => RecvOutcome::Empty,
            Err(err) => {
                tracing::warn!(error = %err, "shared frame skipped");
                RecvOutcome::Empty
            }
        }
    }
}

fn import(
    frame: WireFrame,
    fds: &mut impl Iterator<Item = OwnedFd>,
    release: &Arc<Release>,
    maps: &Arc<MapCache>,
) -> Result<FrameLease, IpcError> {
    let count = frame.backing.fd_count();
    let mut own: Vec<OwnedFd> = fds.by_ref().take(count).collect();
    if own.len() != count {
        return Err(IpcError::Malformed("descriptors missing"));
    }
    let layouts: SmallVec<[PlaneLayout; 3]> = frame.layouts.iter().copied().collect();
    let imported = match frame.backing {
        WireBacking::Memfd { .. } => {
            FrameLease::from_memfd(frame.meta, layouts.clone(), own.remove(0))
        }
        WireBacking::Dmabuf(planes) => {
            let planes: Vec<FrameFdPlane> = own
                .into_iter()
                .zip(planes)
                .map(|(fd, (offset, len))| FrameFdPlane { fd, offset, len })
                .collect();
            if planes.len() != layouts.len() {
                return Err(FrameExportError::PlaneCountMismatch {
                    expected: layouts.len(),
                    actual: planes.len(),
                }
                .into());
            }
            // Planes on one buffer (the usual case) read through the cached mappings.
            match CachedDmabuf::new(maps, planes) {
                Ok(cached) => {
                    let mut meta = frame.meta;
                    meta.residency = Some(FrameResidency::Dmabuf);
                    FrameLease::from_external(meta, layouts.clone(), Arc::new(cached))
                }
                Err(planes) => FrameLease::from_dmabuf(frame.meta, layouts.clone(), planes)?,
            }
        }
    };
    let inner = imported
        .external_backing_handle()
        .ok_or(IpcError::Malformed("frame without backing"))?;
    let meta = imported.meta().clone();
    drop(imported);
    let mut out = FrameLease::from_external(
        meta,
        layouts,
        Arc::new(Released {
            inner,
            _release: release.clone(),
        }),
    );
    for (kind, companion) in frame.companions {
        out = out
            .with_companion(kind, import(companion, fds, release, maps)?)
            .map_err(|_| IpcError::Malformed("companion does not match its frame"))?;
    }
    Ok(out)
}

/// Tells the server a frame was dropped, so it can let go of its buffers.
struct Release {
    socket: Arc<OwnedFd>,
    id: u64,
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = socket::send(&self.socket, &wire::encode_release(self.id), &[]);
    }
}

/// A received frame's memory; the frame is released on the server once it and its companions
/// are all dropped.
struct Released {
    inner: Arc<dyn ExternalBacking>,
    _release: Arc<Release>,
}

impl ExternalBacking for Released {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.inner.plane_data(index)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.inner.backing_bytes()
    }

    fn backing_kind(&self) -> &'static str {
        "ipc"
    }

    fn can_export(&self) -> bool {
        self.inner.can_export()
    }

    fn residency(&self) -> FrameResidency {
        self.inner.residency()
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        self.inner.export_backing()
    }
}
