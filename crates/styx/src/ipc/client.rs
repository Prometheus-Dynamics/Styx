//! Receiving frames in another process.
//!
//! - Blocking: [`FrameClient::recv`] on a thread of its own.
//! - Without a thread (`poll.rs`): the client's descriptor ([`AsFd`](std::os::fd::AsFd)) is
//!   readable when [`FrameClient::try_next`] has something; [`FrameClient::poll_next`],
//!   [`FrameClient::next`] and [`FrameClient::stream`] await frames on any executor.
//! - Controls (`control.rs`): set, read and list the camera's controls, and follow changes.

mod control;
mod import;
mod poll;

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_core::prelude::*;

use styx_core::metrics::HopCounters;

pub use self::control::{AfMode, ControlEvents};
use self::import::{Release, import};
use self::poll::PollSet;
pub use self::poll::{FrameStream, NextFrame};
use super::mapcache::MapCache;
use super::wire::{self, CameraInfo, ClientHops, ClientToken, ServerMessage, WireFrame};
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
///
/// Receive on a thread of its own ([`FrameClient::recv`]), or without one: the client is a
/// file descriptor ([`AsFd`](std::os::fd::AsFd)) that is readable when
/// [`FrameClient::try_next`] has a frame (or news: closed, reconnecting), for a `poll`/`epoll`
/// loop over many clients; or await frames on any executor ([`FrameClient::next`],
/// [`FrameClient::poll_next`], [`FrameClient::stream`]).
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
    /// Where it connected (control connections go there too), and how long opening may take.
    path: PathBuf,
    timeout: Duration,
    /// Readiness for consumers without a thread of their own.
    poll: PollSet,
    /// The control connection, opened on first use.
    control: Mutex<Option<control::ControlLink>>,
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

/// A camera service's answer to a request: the plan, the frames, the client's id and token.
type Accepted = (String, Delivered, Option<ClientToken>);

struct Link {
    socket: Option<Arc<OwnedFd>>,
    plan: Option<String>,
    delivered: Option<Delivered>,
    token: Option<ClientToken>,
    next_attempt: Instant,
    backoff: Duration,
    /// A reconnection made without blocking ([`FrameClient::try_next`]): its socket, waiting
    /// for the service's answer until the deadline.
    pending: Option<(OwnedFd, Instant)>,
    #[cfg(feature = "async")]
    async_fd: Option<Arc<tokio::io::unix::AsyncFd<OwnedFd>>>,
}

impl Link {
    fn connected(&mut self, socket: OwnedFd, accepted: Option<Accepted>, poll: &PollSet) {
        poll.watch(&socket);
        poll.wake_at(None);
        if let Some(old) = self.socket.take() {
            poll.unwatch(&old);
        }
        self.socket = Some(Arc::new(socket));
        (self.plan, self.delivered, self.token) =
            accepted.map_or((None, None, None), |(p, d, t)| (Some(p), Some(d), t));
        self.backoff = RETRY_MIN;
        #[cfg(feature = "async")]
        {
            self.async_fd = None;
        }
    }

    /// The connection is gone: a reconnecting client's descriptor wakes for the next attempt,
    /// another's stays readable (receives return `Closed`).
    fn lost(&mut self, poll: &PollSet, reconnect: bool) {
        if let Some(socket) = self.socket.take() {
            // Frames still held keep the socket open: it must leave the poll set now.
            poll.unwatch(&socket);
        }
        poll.wake_at(Some(if reconnect {
            self.next_attempt
        } else {
            Instant::now()
        }));
        #[cfg(feature = "async")]
        {
            self.async_fd = None;
        }
    }

    /// Schedule the next attempt after a failed one.
    fn failed(&mut self, poll: &PollSet) {
        if let Some((socket, _)) = self.pending.take() {
            poll.unwatch(&socket);
        }
        self.next_attempt = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(RETRY_MAX);
        poll.wake_at(Some(self.next_attempt));
    }
}

/// A camera service's answer to a request: the socket, and the plan and frames it accepted.
type Opened = (OwnedFd, Accepted);

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

fn accepted(message: ServerMessage) -> Result<Accepted, IpcError> {
    match message {
        ServerMessage::Accept(plan, delivered, token) => Ok((plan, *delivered, token)),
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

/// The service's answer (frames and events before it are skipped), waiting until `deadline`.
fn answer(socket: &OwnedFd, deadline: Instant) -> Result<ServerMessage, IpcError> {
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err(IpcError::Io(std::io::ErrorKind::TimedOut.into()));
        }
        match socket::recv(socket, wait)? {
            socket::Received::Message(bytes, _) => match wire::decode_server(&bytes)? {
                ServerMessage::Frame | ServerMessage::ControlEvent(_) => {}
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
    /// limit for each reconnection attempt and each control request.
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
        FrameClient::new(self, socket, Some(accepted), Some(request))
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
        FrameClient::new(self, socket, Some(accepted), Some(request))
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
        FrameClient::new(self, socket, None, None)
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
        options: &ClientOptions,
        socket: OwnedFd,
        accepted: Option<Accepted>,
        request: Option<Request>,
    ) -> Result<Self, IpcError> {
        let poll = PollSet::new()?;
        let mut link = Link {
            socket: None,
            plan: None,
            delivered: None,
            token: None,
            next_attempt: Instant::now(),
            backoff: RETRY_MIN,
            pending: None,
            #[cfg(feature = "async")]
            async_fd: None,
        };
        link.connected(socket, accepted, &poll);
        Ok(Self {
            link: Mutex::new(link),
            maps: Arc::new(MapCache::default()),
            reconnect: options.reconnect && request.is_some(),
            request: request.map(Mutex::new),
            reconnects: AtomicU64::new(0),
            scratch: Mutex::new(Scratch::default()),
            hops: HopCounters::new(),
            path: options.path.clone(),
            timeout: options.timeout,
            poll,
            control: Mutex::new(None),
        })
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

    /// This client's id on its camera (the latest, after reconnecting), as control events name
    /// the client that changed a control ([`ControlEvent::by`](super::ControlEvent::by)).
    /// `None` for a [`FrameServer`](super::FrameServer)'s frames, or from a service that
    /// predates controls.
    pub fn client_id(&self) -> Option<u64> {
        self.link.lock().token.map(|t| t.id)
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
            match self.receive(&socket, remaining) {
                Some(outcome) => return outcome,
                None if !self.reconnect => return RecvOutcome::Closed,
                None => {}
            }
        }
    }

    /// One message from `socket`, waiting up to `wait`: a frame or `Empty`; `None` when the
    /// connection is gone (forgotten here).
    fn receive(&self, socket: &Arc<OwnedFd>, wait: Duration) -> Option<RecvOutcome<FrameLease>> {
        // Another thread receiving at the same time gets buffers of its own.
        let mut own = None;
        let mut guard = self.scratch.try_lock();
        let scratch = match guard.as_deref_mut() {
            Some(s) => s,
            None => own.insert(Scratch::default()),
        };
        scratch.bytes.clear();
        scratch.fds.clear();
        match socket::recv_into(socket, wait, &mut scratch.bytes, &mut scratch.fds) {
            Ok(socket::Got::Message) => Some(self.frame(socket, scratch)),
            Ok(socket::Got::Nothing) => Some(RecvOutcome::Empty),
            Ok(socket::Got::Closed) | Err(_) => {
                drop(guard);
                let mut link = self.link.lock();
                // Unless another thread already reconnected.
                if link.socket.as_ref().is_some_and(|s| Arc::ptr_eq(s, socket)) {
                    link.lost(&self.poll, self.reconnect);
                }
                None
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
                self.link
                    .lock()
                    .connected(socket, Some(accepted), &self.poll);
                self.reconnects.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(err) => {
                crate::trace::debug!(error = %err, "camera service not back yet");
                self.link.lock().failed(&self.poll);
                false
            }
        }
    }

    /// Await the next frame on Tokio; `Closed` once the server is gone (unless reconnecting).
    /// [`FrameClient::next`] does the same on any executor.
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
            match self.receive(&socket, Duration::ZERO) {
                Some(RecvOutcome::Data(frame)) => return RecvOutcome::Data(frame),
                Some(RecvOutcome::Empty) => ready.clear_ready(),
                Some(RecvOutcome::Closed) => return RecvOutcome::Closed,
                None if !self.reconnect => return RecvOutcome::Closed,
                None => {}
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
                self.link
                    .lock()
                    .connected(socket, Some(accepted), &self.poll);
                self.reconnects.fetch_add(1, Ordering::Relaxed);
            }
            _ => self.link.lock().failed(&self.poll),
        }
    }

    /// The frame message in `scratch` imported over its descriptors: one allocation (the
    /// frame's release record) for a frame on one dma-buf. Other messages: `Empty`.
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
