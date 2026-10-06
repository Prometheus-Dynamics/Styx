//! Receiving frames in another process.
//!
//! - Blocking: [`FrameClient::recv`] on a thread of its own.
//! - Without a thread (`poll.rs`): the client's descriptor ([`AsFd`](std::os::fd::AsFd)) is
//!   readable when [`FrameClient::try_next`] has something; [`FrameClient::poll_next`],
//!   [`FrameClient::next`] and [`FrameClient::stream`] await frames on any executor.
//! - Controls (`control.rs`): set, read and list the camera's controls, and follow changes;
//!   [`ControlClient`] (`control_client.rs`) does that without taking frames.
//! - Connecting without blocking (`dial.rs`): [`ClientOptions::request_nonblocking`] and
//!   reconnecting clients connect in steps driven by the client's descriptor.

mod control;
mod control_client;
mod dial;
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

use self::control::ControlChannel;
pub use self::control::{AfMode, ControlEvents};
pub use self::control_client::{ControlClient, ControlEventStream, NextEvent, Ready};
use self::dial::{Dialer, Step};
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
    /// Readiness for consumers without a thread of their own.
    poll: PollSet,
    /// The control connection (opened on first use), to where the client connected.
    control: ControlChannel,
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
    /// Connecting and reconnecting without blocking ([`FrameClient::try_next`]).
    dial: Dialer,
    #[cfg(feature = "async")]
    async_fd: Option<Arc<tokio::io::unix::AsyncFd<OwnedFd>>>,
}

impl Link {
    /// Connected (again): whether it was a reconnection (not the first connection of a client
    /// made with [`ClientOptions::request_nonblocking`]).
    fn connected(&mut self, socket: OwnedFd, accepted: Option<Accepted>, poll: &PollSet) -> bool {
        poll.watch(&socket);
        poll.wake_at(None);
        if let Some(old) = self.socket.take() {
            poll.unwatch(&old);
        }
        self.socket = Some(Arc::new(socket));
        (self.plan, self.delivered, self.token) =
            accepted.map_or((None, None, None), |(p, d, t)| (Some(p), Some(d), t));
        #[cfg(feature = "async")]
        {
            self.async_fd = None;
        }
        !self.dial.succeeded()
    }

    /// The connection is gone: a reconnecting client's descriptor wakes for the next attempt,
    /// another's stays readable (receives return `Closed`).
    fn lost(&mut self, poll: &PollSet, reconnect: bool) {
        if let Some(socket) = self.socket.take() {
            // Frames still held keep the socket open: it must leave the poll set now.
            poll.unwatch(&socket);
        }
        poll.wake_at(Some(if reconnect {
            self.dial.next_attempt
        } else {
            Instant::now()
        }));
        #[cfg(feature = "async")]
        {
            self.async_fd = None;
        }
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
    /// limit for each reconnection attempt and each control request, and how long a
    /// [non-blocking](ClientOptions::request_nonblocking) client that does not reconnect tries
    /// to connect.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Reconnect when the service goes away ([`FrameClient::reconnecting`],
    /// [`ControlClient::reconnecting`]). A [non-blocking](ClientOptions::request_nonblocking)
    /// client also keeps trying until its first connection, however long that takes.
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
    /// Blocks until the service answers (up to the [timeout](ClientOptions::timeout));
    /// [`ClientOptions::request_nonblocking`] does not.
    pub fn request<R: Clone + Into<FrameRequest>>(
        &self,
        request: &R,
    ) -> Result<FrameClient, IpcError> {
        let request = self.request_for(request.clone().into());
        let (socket, accepted) = open(&request)?;
        FrameClient::new(self, Some((socket, accepted)), Some(request))
    }

    /// [`ClientOptions::request`] without waiting: the client comes back at once, connecting
    /// in the background, even when the service is not there yet. Until the service accepts
    /// the request, [`FrameClient::try_next`] returns `Empty` (not an error) and
    /// [`FrameClient::is_connected`] is false; the client's descriptor
    /// ([`AsFd`](std::os::fd::AsFd)) becomes readable when the answer comes or the next
    /// attempt is due, and frames follow. `client.ready().await` waits for the connection on
    /// any executor ([`FrameClient::ready`]).
    ///
    /// Not [reconnecting](ClientOptions::reconnecting), the client gives up when the service
    /// refuses the request or has not accepted it within the
    /// [timeout](ClientOptions::timeout): receives then return `Closed`, and
    /// [`FrameClient::last_error`] says why. Reconnecting, it keeps trying (backing off from
    /// 100 ms to 2 s), and after the service accepted it, reconnects as
    /// [`FrameClient::reconnecting`] does. Fails only when the client's own descriptors cannot
    /// be made.
    pub fn request_nonblocking<R: Clone + Into<FrameRequest>>(
        &self,
        request: &R,
    ) -> Result<FrameClient, IpcError> {
        let request = self.request_for(request.clone().into());
        let client = FrameClient::new(self, None, Some(request))?;
        client.poll.clear_timer();
        client.reconnect_step();
        Ok(client)
    }

    /// A [`ControlClient`] for the camera service's camera (the one
    /// [`ClientOptions::camera`] names, else its first): controls only, no frames. Blocks
    /// until the service has answered (up to the timeout); fails when it is not there or has
    /// no such camera. [`ClientOptions::controls_nonblocking`] does not wait.
    pub fn controls(&self) -> Result<ControlClient, IpcError> {
        ControlClient::open(self)
    }

    /// [`ClientOptions::controls`] without waiting: the client comes back at once and
    /// connects in the background ([`ControlClient`] has the details).
    pub fn controls_nonblocking(&self) -> Result<ControlClient, IpcError> {
        ControlClient::start(self)
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
        FrameClient::new(self, Some((socket, accepted)), Some(request))
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
        let client = FrameClient::new(self, None, None)?;
        client.link.lock().connected(socket, None, &client.poll);
        Ok(client)
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

    /// A client connected with `opened`, else one connecting in the background (a request)
    /// or about to be connected by the caller (a frame server's client).
    fn new(
        options: &ClientOptions,
        opened: Option<Opened>,
        request: Option<Request>,
    ) -> Result<Self, IpcError> {
        let poll = PollSet::new()?;
        let connecting = opened.is_none() && request.is_some();
        let mut link = Link {
            socket: None,
            plan: None,
            delivered: None,
            token: None,
            dial: if connecting {
                Dialer::connecting((!options.reconnect).then(|| Instant::now() + options.timeout))
            } else {
                Dialer::connected()
            },
            #[cfg(feature = "async")]
            async_fd: None,
        };
        if let Some((socket, accepted)) = opened {
            link.connected(socket, Some(accepted), &poll);
        }
        Ok(Self {
            link: Mutex::new(link),
            maps: Arc::new(MapCache::default()),
            reconnect: options.reconnect && request.is_some(),
            request: request.map(Mutex::new),
            reconnects: AtomicU64::new(0),
            scratch: Mutex::new(Scratch::default()),
            hops: HopCounters::new(),
            poll,
            control: ControlChannel::new(&options.path, options.timeout),
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
    /// `Closed`. Only for clients of a camera service (made with [`FrameClient::request`] or
    /// [`ClientOptions::request_nonblocking`]).
    pub fn reconnecting(mut self) -> Self {
        self.reconnect = self.request.is_some();
        self
    }

    /// Whether the client has a connection now (false while a
    /// [non-blocking](ClientOptions::request_nonblocking) client is still connecting).
    pub fn is_connected(&self) -> bool {
        self.link.lock().socket.is_some()
    }

    /// Why the last connection attempt failed (a copy; `None` once connected): the service is
    /// not there, timed out, or [rejected](IpcError::Rejected) the request. For clients that
    /// connect in the background ([`ClientOptions::request_nonblocking`], reconnecting).
    pub fn last_error(&self) -> Option<IpcError> {
        self.link.lock().dial.error()
    }

    /// Whether a lost or missing connection is (still) being made: the client reconnects, or
    /// a non-blocking client has not connected or given up yet.
    fn retries(&self) -> bool {
        self.reconnect || self.link.lock().dial.first_pending()
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
    /// buffers until the returned frame (and its companions) are dropped. While the client
    /// (re)connects, it waits no longer than `wait` either: attempts and the service's answer
    /// are awaited on the client's descriptor.
    pub fn recv(&self, wait: Duration) -> RecvOutcome<FrameLease> {
        let deadline = Instant::now() + wait;
        loop {
            let socket = self.link.lock().socket.clone();
            let Some(socket) = socket else {
                if !self.retries() {
                    return RecvOutcome::Closed;
                }
                self.poll.clear_timer();
                if self.reconnect_step() {
                    continue;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return RecvOutcome::Empty;
                }
                self.poll.wait(remaining);
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
                if !self.retries() {
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
        let next = self.link.lock().dial.next_attempt;
        tokio::time::sleep_until(next.into()).await;
        let request = request.lock().clone();
        match open_async(&request).await {
            Ok((socket, accepted)) => {
                if self
                    .link
                    .lock()
                    .connected(socket, Some(accepted), &self.poll)
                {
                    self.reconnects.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(err) => self
                .link
                .lock()
                .dial
                .failed(&self.poll, err, self.reconnect),
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
