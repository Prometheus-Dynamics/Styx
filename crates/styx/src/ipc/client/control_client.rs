//! A camera service client for controls only: no frames, so it never joins a camera's frame
//! plan, holds no buffers and never starts or restarts a capture by connecting or leaving.

use std::future::Future;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_core::prelude::*;

use super::control::{ControlChannel, ControlTo, control_methods};
use super::dial::{Dialer, Step};
use super::news::{ClientEvent, ClientEventStream, EventSource, NextClientEvent, poll_with};
use super::poll::{PollSet, TRY_MESSAGES};
use super::{ClientOptions, FrameClient};
use crate::ipc::controls::ControlEvent;
use crate::ipc::wire::{self, ControlOp, ControlReply, ServerMessage};
use crate::ipc::{IpcError, socket};

/// Sets, reads and lists a camera service's camera controls, and follows their changes,
/// without taking frames.
///
/// Unlike a [`FrameClient`], it makes no frame request: it is not one of the camera's frame
/// clients, so connecting or leaving never changes the camera's plan, starts or restarts its
/// capture, or keeps it from idling, and it holds no buffers. (Setting a frame rate the camera
/// cannot change while streaming still restarts the capture for every client, as the
/// service's [`ControlPolicy`](crate::ipc::ControlPolicy) allows.) The policy applies to it as
/// to any client: it is never the camera's owner, and [`ControlCaller::client`] is `None` for
/// it. A control set while no client takes frames is remembered, and applied when the capture
/// starts (`AppliedControl::deferred`). Control events name it `by: None`.
///
/// It is a file descriptor ([`AsFd`]), readable when [`ControlClient::try_event`] has a
/// control change or news (connected; closed; the next connection attempt is due), for a
/// `poll`/`epoll` loop; [`ControlClient::next_event`], [`ControlClient::events`] and
/// [`ControlClient::ready`] await the same on any executor. Requests wait for the service's
/// answer ([`ControlClient::set_control`], up to the timeout) or are awaited
/// ([`ControlClient::set_control_async`]); they go on a connection of their own, opened on
/// first use, so they work whether or not the event connection is up.
///
/// [`ControlClient::connect`] waits until the service has answered;
/// [`ClientOptions::controls_nonblocking`] does not: the client connects in the background
/// (driven by `try_event`, `next_event` or `ready`), and a [reconnecting](ControlClient::reconnecting)
/// one comes back after the service restarts.
///
/// [`ControlCaller::client`]: crate::ipc::ControlCaller::client
pub struct ControlClient {
    /// Control requests, on a connection of their own.
    control: ControlChannel,
    camera: Option<String>,
    timeout: Duration,
    reconnect: bool,
    reconnects: AtomicU64,
    /// The event subscription: the client's connection.
    events: Mutex<Events>,
    poll: PollSet,
}

struct Events {
    socket: Option<OwnedFd>,
    dial: Dialer,
}

impl ControlClient {
    /// How to connect to the camera service at `path`: camera, timeout, reconnecting
    /// ([`ClientOptions`]; then [`ClientOptions::controls`] or
    /// [`ClientOptions::controls_nonblocking`]).
    pub fn options(path: impl AsRef<Path>) -> ClientOptions {
        FrameClient::options(path)
    }

    /// Controls of the first camera of the [`CameraService`](crate::ipc::CameraService) at
    /// `path`. Waits until the service has answered (up to
    /// [`DEFAULT_OPEN_TIMEOUT`](super::DEFAULT_OPEN_TIMEOUT)); fails at once when it is not
    /// there.
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        Self::options(path).controls()
    }

    /// [`ControlClient::connect`] for the camera `camera` names: its name, part of it, or one
    /// of its identity keys. Fails when the service has no such camera.
    pub fn connect_camera(path: impl AsRef<Path>, camera: &str) -> Result<Self, IpcError> {
        Self::options(path).camera(camera).controls()
    }

    fn new(options: &ClientOptions, dial: Dialer) -> Result<Self, IpcError> {
        Ok(Self {
            control: ControlChannel::new(&options.path, options.timeout),
            camera: options.camera.clone(),
            timeout: options.timeout,
            reconnect: options.reconnect,
            reconnects: AtomicU64::new(0),
            events: Mutex::new(Events { socket: None, dial }),
            poll: PollSet::new()?,
        })
    }

    /// Connected before returning ([`ClientOptions::controls`]).
    pub(super) fn open(options: &ClientOptions) -> Result<Self, IpcError> {
        let client = Self::new(options, Dialer::connecting(None))?;
        let socket = client.control.subscribe(&client.to())?.into_socket();
        client.connected(&mut client.events.lock(), socket);
        Ok(client)
    }

    /// Connecting in the background ([`ClientOptions::controls_nonblocking`]).
    pub(super) fn start(options: &ClientOptions) -> Result<Self, IpcError> {
        let deadline = (!options.reconnect).then(|| Instant::now() + options.timeout);
        let client = Self::new(options, Dialer::connecting(deadline))?;
        client.step(&mut client.events.lock());
        Ok(client)
    }

    /// Reconnect when the service goes away (backing off from 100 ms to 2 s), subscribing
    /// again: [`ControlClient::try_event`] returns `Empty` meanwhile instead of `Closed`.
    pub fn reconnecting(mut self) -> Self {
        self.reconnect = true;
        self
    }

    fn to(&self) -> ControlTo {
        ControlTo {
            camera: self.camera.clone(),
            token: None,
        }
    }

    fn control_to(&self) -> Result<ControlTo, IpcError> {
        Ok(self.to())
    }

    control_methods!();

    /// The camera it controls, as named ([`ClientOptions::camera`]); `None`: the service's
    /// first.
    pub fn camera(&self) -> Option<&str> {
        self.camera.as_deref()
    }

    /// Whether the event connection is up (false while a
    /// [non-blocking](ClientOptions::controls_nonblocking) client is still connecting).
    pub fn is_connected(&self) -> bool {
        self.events.lock().socket.is_some()
    }

    /// Times a [reconnecting](ControlClient::reconnecting) client connected again.
    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }

    /// Why the last connection attempt failed (a copy; `None` once connected): the service is
    /// not there, timed out, or has no such camera ([`IpcError::ControlRefused`]).
    pub fn last_error(&self) -> Option<IpcError> {
        self.events.lock().dial.error()
    }

    fn connected(&self, events: &mut Events, socket: OwnedFd) {
        self.poll.watch(&socket);
        self.poll.wake_at(None);
        events.socket = Some(socket);
        if !events.dial.succeeded(&self.poll) {
            self.reconnects.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether a missing connection is (still) being made.
    fn retries(&self, events: &Events) -> bool {
        self.reconnect || events.dial.first_pending()
    }

    /// One step of (re)connecting without blocking; whether it is connected now.
    fn step(&self, events: &mut Events) -> bool {
        if events.socket.is_some() {
            return true;
        }
        let hello = || wire::encode_control(&self.to().message(1, ControlOp::Subscribe));
        let step = events.dial.step(
            &self.poll,
            self.control.path(),
            self.timeout,
            self.reconnect,
            hello,
            subscribed,
        );
        match step {
            Step::Connected(socket, ()) => {
                self.connected(events, socket);
                true
            }
            Step::Waiting | Step::GaveUp => false,
        }
    }

    /// The next control change if one has arrived, without waiting: `Empty` when none has
    /// (wait for the client's descriptor, [`AsFd`], to become readable), `Closed` once the
    /// service is gone (unless reconnecting) or a client that does not reconnect gave up
    /// connecting ([`ControlClient::last_error`]). Connects and reconnects here, without
    /// blocking.
    pub fn try_event(&self) -> RecvOutcome<ControlEvent> {
        self.take_event(false)
    }

    /// [`ControlClient::try_event`]; with `news`, `Empty` as soon as a connection change waits
    /// (nothing from a new connection is read before its `Connected` is).
    fn take_event(&self, news: bool) -> RecvOutcome<ControlEvent> {
        self.poll.clear_timer();
        for _ in 0..TRY_MESSAGES {
            let mut events = self.events.lock();
            if news && events.dial.news.pending() {
                return RecvOutcome::Empty;
            }
            let Some(socket) = &events.socket else {
                if !self.retries(&events) {
                    // Stay readable: whoever polls learns it is closed.
                    self.poll.wake_at(Some(Instant::now()));
                    return RecvOutcome::Closed;
                }
                if self.step(&mut events) {
                    continue;
                }
                return RecvOutcome::Empty;
            };
            match socket::recv(socket, Duration::ZERO) {
                Ok(socket::Received::Message(bytes, _)) => {
                    if let Ok(ServerMessage::ControlEvent(event)) = wire::decode_server(&bytes) {
                        return RecvOutcome::Data(event);
                    }
                }
                Ok(socket::Received::Nothing) => return RecvOutcome::Empty,
                Ok(socket::Received::Closed) | Err(_) => {
                    if let Some(socket) = events.socket.take() {
                        self.poll.unwatch(&socket);
                    }
                    self.poll.wake_at(Some(if self.reconnect {
                        events.dial.next_attempt
                    } else {
                        Instant::now()
                    }));
                    // Wakes now: the loss is news.
                    let reset = IpcError::Io(io::ErrorKind::ConnectionReset.into());
                    events.dial.lost(&self.poll, reset);
                }
            }
        }
        // More to read: the descriptor is still readable.
        RecvOutcome::Empty
    }

    /// The next control change, waiting up to `wait` (connecting meanwhile, never for longer).
    pub fn recv_event(&self, wait: Duration) -> RecvOutcome<ControlEvent> {
        let deadline = Instant::now() + wait;
        loop {
            match self.try_event() {
                RecvOutcome::Empty => {}
                outcome => return outcome,
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return RecvOutcome::Empty;
            }
            self.poll.wait(left);
        }
    }

    /// Poll for the next control change from a hand-written future: `Ready` with one (or
    /// `Closed`), else `Pending` with `cx`'s waker woken when the client's descriptor becomes
    /// readable (through styx-graph's reactor, so on any executor).
    pub fn poll_event(&self, cx: &mut Context<'_>) -> Poll<RecvOutcome<ControlEvent>> {
        poll_with(&self.poll, cx, || self.try_event())
    }

    /// [`ControlClient::try_event`] with the client's connection changes, in order:
    /// `Connected` (the first connection, also of a client whose blocking `connect` returned
    /// connected, and each reconnection: re-apply settings here), `Disconnected` (the
    /// connection lost, or a client that does not reconnect gave up connecting), and `Data`
    /// with each control change. Each change is reported once, and the descriptor ([`AsFd`])
    /// becomes readable for it; changes received on a connection come after its `Connected`
    /// and before its `Disconnected`. `Closed` after the last `Disconnected` of a client that
    /// does not reconnect. Use it instead of `try_event` (which leaves connection changes
    /// unread), not next to it.
    pub fn try_client_event(&self) -> RecvOutcome<ClientEvent<ControlEvent>> {
        if let Some(change) = self.events.lock().dial.news.pop(&self.poll) {
            return RecvOutcome::Data(change.into());
        }
        match self.take_event(true) {
            RecvOutcome::Data(event) => RecvOutcome::Data(ClientEvent::Data(event)),
            // What this call noticed (connected, lost, gave up), else nothing (yet), or closed.
            outcome => match self.events.lock().dial.news.pop(&self.poll) {
                Some(change) => RecvOutcome::Data(change.into()),
                None if matches!(outcome, RecvOutcome::Closed) => RecvOutcome::Closed,
                None => RecvOutcome::Empty,
            },
        }
    }

    /// [`ControlClient::try_client_event`] from a hand-written future: `Pending` with `cx`'s
    /// waker woken when the client's descriptor becomes readable (any executor).
    pub fn poll_client_event(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<RecvOutcome<ClientEvent<ControlEvent>>> {
        poll_with(&self.poll, cx, || self.try_client_event())
    }

    /// Await the next control or connection change on any executor.
    pub fn next_client_event(&self) -> NextClientEvent<'_, ControlEvent> {
        NextClientEvent { client: self }
    }

    /// Control and connection changes as a [`Stream`](futures_core::Stream), ending when the
    /// client is closed (never for a reconnecting one).
    pub fn client_events(&self) -> ClientEventStream<'_, ControlEvent> {
        ClientEventStream {
            client: self,
            done: false,
        }
    }

    /// Await the next control change on any executor; `Closed` once the service is gone
    /// (unless reconnecting).
    pub fn next_event(&self) -> NextEvent<'_> {
        NextEvent { client: self }
    }

    /// The control changes as a [`Stream`](futures_core::Stream), ending when the service is
    /// gone (unless reconnecting).
    pub fn events(&self) -> ControlEventStream<'_> {
        ControlEventStream {
            client: self,
            done: false,
        }
    }

    /// Poll for the connection from a hand-written future: `Ready(Ok)` once connected,
    /// `Ready(Err)` when a client that does not reconnect gave up (why), else `Pending`.
    /// Takes no events.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), IpcError>> {
        loop {
            self.poll.clear_timer();
            {
                let mut events = self.events.lock();
                if self.step(&mut events) {
                    return Poll::Ready(Ok(()));
                }
                if !self.retries(&events) {
                    let err = events
                        .dial
                        .error()
                        .unwrap_or_else(|| IpcError::Io(io::ErrorKind::NotConnected.into()));
                    return Poll::Ready(Err(err));
                }
            }
            match self.poll.reactor() {
                Ok(reactor) => match reactor.poll_read_ready(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err.into())),
                },
                Err(err) => return Poll::Ready(Err(err.into())),
            }
        }
    }

    /// Wait until the client is connected, on any executor (an error when it gave up). With
    /// [`ClientOptions::controls_nonblocking`], the async form of [`ControlClient::connect`].
    pub fn ready(&self) -> Ready<'_> {
        Ready(Waiting::Controls(self))
    }
}

impl EventSource<ControlEvent> for ControlClient {
    fn poll_client_event(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<RecvOutcome<ClientEvent<ControlEvent>>> {
        ControlClient::poll_client_event(self, cx)
    }
}

/// The service's answer to a subscription; `None`: skip the message.
fn subscribed(message: ServerMessage) -> Option<Result<(), IpcError>> {
    match message {
        ServerMessage::ControlReply(1, ControlReply::Subscribed) => Some(Ok(())),
        ServerMessage::ControlReply(_, ControlReply::Refused(refusal)) => {
            Some(Err(IpcError::ControlRefused(refusal)))
        }
        ServerMessage::Reject(reason) => Some(Err(IpcError::Rejected(reason))),
        ServerMessage::ControlEvent(_) => None,
        _ => Some(Err(IpcError::Malformed("expected a subscription"))),
    }
}

/// The client's descriptor: readable when [`ControlClient::try_event`] has a control change
/// or news. It stays the same across reconnections.
impl AsFd for ControlClient {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.poll.fd()
    }
}

impl AsRawFd for ControlClient {
    fn as_raw_fd(&self) -> RawFd {
        self.poll.fd().as_raw_fd()
    }
}

/// [`FrameClient::ready`], [`ControlClient::ready`]: the client is connected.
#[must_use = "futures do nothing unless polled"]
pub struct Ready<'a>(pub(super) Waiting<'a>);

pub(super) enum Waiting<'a> {
    Frames(&'a FrameClient),
    Controls(&'a ControlClient),
}

impl Future for Ready<'_> {
    type Output = Result<(), IpcError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0 {
            Waiting::Frames(client) => client.poll_ready(cx),
            Waiting::Controls(client) => client.poll_ready(cx),
        }
    }
}

/// [`ControlClient::next_event`].
#[must_use = "futures do nothing unless polled"]
pub struct NextEvent<'a> {
    client: &'a ControlClient,
}

impl Future for NextEvent<'_> {
    type Output = RecvOutcome<ControlEvent>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.client.poll_event(cx)
    }
}

/// [`ControlClient::events`].
#[must_use = "streams do nothing unless polled"]
pub struct ControlEventStream<'a> {
    client: &'a ControlClient,
    done: bool,
}

impl futures_core::Stream for ControlEventStream<'_> {
    type Item = ControlEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<ControlEvent>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.client.poll_event(cx) {
            Poll::Ready(RecvOutcome::Data(event)) => Poll::Ready(Some(event)),
            Poll::Ready(_) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
