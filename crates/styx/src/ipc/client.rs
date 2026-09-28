//! Receiving frames in another process.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smallvec::SmallVec;
use styx_core::prelude::*;

use super::wire::{self, CameraInfo, ServerMessage, WireBacking, WireFrame};
use super::{IpcError, socket};

/// How long a request waits for the service's answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Backoff between reconnection attempts.
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(2);

/// Receives frames from a [`CameraService`](super::CameraService) or a
/// [`FrameServer`](super::FrameServer) in another process.
pub struct FrameClient {
    link: Mutex<Link>,
    /// What to ask a camera service for again after reconnecting (with the latest ROI).
    request: Option<Mutex<Request>>,
    reconnect: bool,
    reconnects: AtomicU64,
}

#[derive(Clone)]
struct Request {
    path: PathBuf,
    camera: Option<String>,
    requirements: FrameRequirements,
}

struct Link {
    socket: Option<Arc<OwnedFd>>,
    plan: Option<String>,
    next_attempt: Instant,
    backoff: Duration,
    #[cfg(feature = "async")]
    async_fd: Option<Arc<tokio::io::unix::AsyncFd<OwnedFd>>>,
}

impl Link {
    fn connected(&mut self, socket: OwnedFd, plan: Option<String>) {
        self.socket = Some(Arc::new(socket));
        self.plan = plan;
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

/// Connect to a camera service and make `request`; the socket and the plan it answered with.
fn open(request: &Request) -> Result<(OwnedFd, String), IpcError> {
    let socket = socket::connect(&request.path)?;
    socket::send(
        &socket,
        &wire::encode_request(&request.requirements, request.camera.as_deref()),
        &[],
    )?;
    match answer(&socket)? {
        ServerMessage::Accept(plan) => Ok((socket, plan)),
        ServerMessage::Reject(reason) => Err(IpcError::Rejected(reason)),
        _ => Err(IpcError::Malformed("expected an answer to the request")),
    }
}

/// The service's answer to a request (frames before it are skipped).
fn answer(socket: &OwnedFd) -> Result<ServerMessage, IpcError> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
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

impl FrameClient {
    /// Connect to a [`FrameServer`](super::FrameServer).
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        Ok(Self::new(socket::connect(path.as_ref())?, None, None))
    }

    /// Ask the [`CameraService`](super::CameraService) at `path` for frames that meet
    /// `requirements`, from its first camera. Fails with [`IpcError::Rejected`] (and the
    /// planner's reasons) when the camera cannot serve them next to its other clients.
    pub fn request(
        path: impl AsRef<Path>,
        requirements: &FrameRequirements,
    ) -> Result<Self, IpcError> {
        Self::request_from(path, None, requirements)
    }

    /// [`FrameClient::request`] from the camera `camera` names: its name, part of it, or one
    /// of its identity keys (see [`FrameClient::cameras`]).
    pub fn request_camera(
        path: impl AsRef<Path>,
        camera: &str,
        requirements: &FrameRequirements,
    ) -> Result<Self, IpcError> {
        Self::request_from(path, Some(camera), requirements)
    }

    fn request_from(
        path: impl AsRef<Path>,
        camera: Option<&str>,
        requirements: &FrameRequirements,
    ) -> Result<Self, IpcError> {
        let request = Request {
            path: path.as_ref().to_path_buf(),
            camera: camera.map(str::to_owned),
            requirements: requirements.clone(),
        };
        let (socket, plan) = open(&request)?;
        Ok(Self::new(socket, Some(plan), Some(request)))
    }

    /// The cameras the [`CameraService`](super::CameraService) at `path` serves.
    pub fn cameras(path: impl AsRef<Path>) -> Result<Vec<CameraInfo>, IpcError> {
        let socket = socket::connect(path.as_ref())?;
        socket::send(&socket, &wire::encode_list(), &[])?;
        match answer(&socket)? {
            ServerMessage::Cameras(cameras) => Ok(cameras),
            ServerMessage::Reject(reason) => Err(IpcError::Rejected(reason)),
            _ => Err(IpcError::Malformed("expected a camera list")),
        }
    }

    fn new(socket: OwnedFd, plan: Option<String>, request: Option<Request>) -> Self {
        Self {
            link: Mutex::new(Link {
                socket: Some(Arc::new(socket)),
                plan,
                next_attempt: Instant::now(),
                backoff: RETRY_MIN,
                #[cfg(feature = "async")]
                async_fd: None,
            }),
            request: request.map(Mutex::new),
            reconnect: false,
            reconnects: AtomicU64::new(0),
        }
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
            request.lock().requirements.roi = roi;
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
            Ok((socket, plan)) => {
                self.link.lock().connected(socket, Some(plan));
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

    /// Wait for the next attempt, then connect again off the async runtime's threads.
    #[cfg(feature = "async")]
    async fn reconnect_async(&self) {
        let Some(request) = &self.request else {
            return;
        };
        let next = self.link.lock().next_attempt;
        tokio::time::sleep_until(next.into()).await;
        let request = request.lock().clone();
        match tokio::task::spawn_blocking(move || open(&request)).await {
            Ok(Ok((socket, plan))) => {
                self.link.lock().connected(socket, Some(plan));
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
                import(*frame, &mut fds.into_iter(), &release).map(Some)
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
            let planes = own
                .into_iter()
                .zip(planes)
                .map(|(fd, (offset, len))| FrameFdPlane { fd, offset, len })
                .collect();
            FrameLease::from_dmabuf(frame.meta, layouts.clone(), planes)?
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
            .with_companion(kind, import(companion, fds, release)?)
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
