//! A [`FrameClient`] without a thread of its own.
//!
//! The client owns an epoll descriptor holding its connection (and, while it reconnects, the
//! new connection or a timer for the next attempt): readable exactly when
//! [`FrameClient::try_next`] has something to do. A `poll`/`epoll` loop (or any reactor) waits
//! on many clients' descriptors at once. [`FrameClient::poll_next`] registers the same
//! descriptor with styx-graph's reactor (one thread for the process, started on first use) and
//! wakes the task's waker, so frames can be awaited on any executor, or none
//! (`styx_graph::rt::block_on`).

use std::future::Future;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use styx_core::prelude::*;
use styx_graph::rt::AsyncFd;

use super::control_client::{Ready, Waiting};
use super::{FrameClient, ServerMessage, Step, accepted};
use crate::ipc::{IpcError, socket, wire};

/// An epoll set with the client's sockets, and a timer for reconnection attempts.
pub(super) struct PollSet {
    epoll: OwnedFd,
    timer: OwnedFd,
    /// The epoll descriptor registered with styx-graph's reactor (a duplicate), on first use.
    reactor: OnceLock<io::Result<AsyncFd<OwnedFd>>>,
}

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

impl PollSet {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: plain syscalls; non-negative results are new descriptors we own.
        let epoll =
            unsafe { OwnedFd::from_raw_fd(check(libc::epoll_create1(libc::EPOLL_CLOEXEC))?) };
        // SAFETY: as above.
        let timer = unsafe {
            OwnedFd::from_raw_fd(check(libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
            ))?)
        };
        let set = Self {
            epoll,
            timer,
            reactor: OnceLock::new(),
        };
        set.ctl(libc::EPOLL_CTL_ADD, set.timer.as_raw_fd())?;
        Ok(set)
    }

    fn ctl(&self, op: libc::c_int, fd: RawFd) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: fd as u64,
        };
        // SAFETY: valid epoll and target descriptors, and a valid event.
        check(unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), op, fd, &mut event) }).map(drop)
    }

    /// Wake when `socket` is readable (or closed).
    pub(super) fn watch(&self, socket: &OwnedFd) {
        match self.ctl(libc::EPOLL_CTL_ADD, socket.as_raw_fd()) {
            Ok(()) => {}
            Err(err) if err.raw_os_error() == Some(libc::EEXIST) => {}
            Err(err) => crate::trace::warn!(error = %err, "client socket not polled"),
        }
    }

    pub(super) fn unwatch(&self, socket: &OwnedFd) {
        let _ = self.ctl(libc::EPOLL_CTL_DEL, socket.as_raw_fd());
    }

    /// Wake at `at` (now if it has passed); `None`: not for the timer.
    pub(super) fn wake_at(&self, at: Option<Instant>) {
        let after = at.map(|at| {
            at.saturating_duration_since(Instant::now())
                .max(Duration::from_nanos(1))
        });
        let ts = |d: Duration| libc::timespec {
            tv_sec: d.as_secs() as _,
            tv_nsec: libc::c_long::from(d.subsec_nanos() as i32),
        };
        let spec = libc::itimerspec {
            it_interval: ts(Duration::ZERO),
            it_value: ts(after.unwrap_or(Duration::ZERO)),
        };
        // SAFETY: a valid timerfd and timer value; the old value is not asked for.
        let _ = unsafe {
            libc::timerfd_settime(self.timer.as_raw_fd(), 0, &spec, std::ptr::null_mut())
        };
    }

    /// The epoll descriptor: readable when the client has something to do.
    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.epoll.as_fd()
    }

    /// Wait up to `wait` for the set to be readable (for blocking receives).
    pub(super) fn wait(&self, wait: Duration) {
        socket::readable(&self.epoll, wait);
    }

    /// The timer fired: not readable for it any more.
    pub(super) fn clear_timer(&self) {
        let mut expirations = 0u64;
        // SAFETY: reads 8 bytes into a u64 from a non-blocking timerfd.
        let _ = unsafe {
            libc::read(
                self.timer.as_raw_fd(),
                (&raw mut expirations).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }

    pub(super) fn reactor(&self) -> io::Result<&AsyncFd<OwnedFd>> {
        self.reactor
            .get_or_init(|| AsyncFd::new(self.epoll.try_clone()?))
            .as_ref()
            .map_err(|err| io::Error::new(err.kind(), err.to_string()))
    }
}

/// The client's descriptor: readable when [`FrameClient::try_next`] has a frame, or has news
/// (the connection closed; a reconnecting client's next attempt is due or its answer came).
/// It stays the same across reconnections. Wait on it with `poll`/`epoll` (or register it
/// with any reactor), then call [`FrameClient::try_next`] until it returns `Empty`.
impl AsFd for FrameClient {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.poll.fd()
    }
}

impl AsRawFd for FrameClient {
    fn as_raw_fd(&self) -> RawFd {
        self.poll.fd().as_raw_fd()
    }
}

/// Most messages [`FrameClient::try_next`] reads before giving the caller its turn back.
pub(super) const TRY_MESSAGES: usize = 8;

impl FrameClient {
    /// The next frame if one has arrived, without waiting: `Empty` when none has (wait for the
    /// client's descriptor, [`AsFd`], to become readable), `Closed` once the server is gone
    /// (unless reconnecting). A [reconnecting](FrameClient::reconnecting) client reconnects
    /// here without blocking either: an attempt when it is due, the service's answer when it
    /// comes. Allocates nothing per frame beyond the frame's release record.
    pub fn try_next(&self) -> RecvOutcome<FrameLease> {
        self.poll.clear_timer();
        for _ in 0..TRY_MESSAGES {
            let socket = self.link.lock().socket.clone();
            let Some(socket) = socket else {
                if !self.retries() {
                    // Stay readable: whoever polls learns it is closed.
                    self.poll.wake_at(Some(Instant::now()));
                    return RecvOutcome::Closed;
                }
                if self.reconnect_step() {
                    continue;
                }
                return RecvOutcome::Empty;
            };
            match self.receive(&socket, Duration::ZERO) {
                Some(RecvOutcome::Data(frame)) => return RecvOutcome::Data(frame),
                Some(RecvOutcome::Closed) => return RecvOutcome::Closed,
                // Nothing there (or a message that is not a frame, read again).
                Some(RecvOutcome::Empty) => {
                    if !socket::readable(&socket, Duration::ZERO) {
                        return RecvOutcome::Empty;
                    }
                }
                None if !self.reconnect => {
                    self.poll.wake_at(Some(Instant::now()));
                    return RecvOutcome::Closed;
                }
                None => {}
            }
        }
        // More to read: the descriptor is still readable.
        RecvOutcome::Empty
    }

    /// One step of (re)connecting without blocking: start an attempt when one is due, or take
    /// the service's answer to the attempt under way. Whether the client is connected now.
    pub(super) fn reconnect_step(&self) -> bool {
        let Some(request) = &self.request else {
            return false;
        };
        let request = request.lock().clone();
        let mut link = self.link.lock();
        if link.socket.is_some() {
            return true;
        }
        let step = link.dial.step(
            &self.poll,
            &request.path,
            request.timeout,
            self.reconnect,
            || wire::encode_request(&request.frames, request.camera.as_deref()),
            |message| match message {
                ServerMessage::Frame | ServerMessage::ControlEvent(_) => None,
                message => Some(accepted(message)),
            },
        );
        match step {
            Step::Connected(socket, accepted) => {
                if link.connected(socket, Some(accepted), &self.poll) {
                    self.reconnects
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                true
            }
            Step::Waiting | Step::GaveUp => false,
        }
    }

    /// Poll for the client's connection from a hand-written future: `Ready(Ok)` once it is
    /// connected, `Ready(Err)` when a client that does not reconnect gave up (why), else
    /// `Pending` with `cx`'s waker woken when the client's descriptor becomes readable. Drives
    /// a [non-blocking](super::ClientOptions::request_nonblocking) client's connection without
    /// taking frames.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), IpcError>> {
        loop {
            self.poll.clear_timer();
            if self.is_connected() || self.reconnect_step() {
                return Poll::Ready(Ok(()));
            }
            {
                let link = self.link.lock();
                if link.socket.is_none() && !self.reconnect && !link.dial.first_pending() {
                    let err = link
                        .dial
                        .error()
                        .unwrap_or_else(|| IpcError::Io(io::ErrorKind::NotConnected.into()));
                    return Poll::Ready(Err(err));
                }
            }
            let reactor = match self.poll.reactor() {
                Ok(reactor) => reactor,
                Err(err) => return Poll::Ready(Err(err.into())),
            };
            match reactor.poll_read_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err.into())),
            }
        }
    }

    /// Wait until the client is connected, on any executor: at once for a connected client;
    /// a [non-blocking](super::ClientOptions::request_nonblocking) one once the service
    /// accepted its request (an error when it gave up). With
    /// [`ClientOptions::request_nonblocking`](super::ClientOptions::request_nonblocking), the
    /// async form of [`FrameClient::request`]:
    /// `let client = options.request_nonblocking(&frames)?; client.ready().await?;`.
    pub fn ready(&self) -> Ready<'_> {
        Ready(Waiting::Frames(self))
    }

    /// Poll for the next frame from a hand-written future or stream: `Ready` with a frame (or
    /// `Closed`), else `Pending` with `cx`'s waker woken when the client's descriptor becomes
    /// readable. Waits through styx-graph's reactor, so it works on any executor. One task
    /// polls a client at a time (the last one to poll is woken).
    pub fn poll_next(&self, cx: &mut Context<'_>) -> Poll<RecvOutcome<FrameLease>> {
        loop {
            match self.try_next() {
                RecvOutcome::Empty => {}
                outcome => return Poll::Ready(outcome),
            }
            let reactor = match self.poll.reactor() {
                Ok(reactor) => reactor,
                Err(err) => {
                    crate::trace::warn!(error = %err, "no reactor for the frame client");
                    return Poll::Ready(RecvOutcome::Closed);
                }
            };
            match reactor.poll_read_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(_)) => return Poll::Ready(RecvOutcome::Closed),
            }
        }
    }

    /// Await the next frame on any executor (or `styx_graph::rt::block_on`); `Closed` once
    /// the server is gone (unless reconnecting). [`FrameClient::stream`] for a `Stream`.
    pub fn next(&self) -> NextFrame<'_> {
        NextFrame { client: self }
    }

    /// The client's frames as a [`Stream`](futures_core::Stream), ending when the server is
    /// gone (unless reconnecting).
    pub fn stream(&self) -> FrameStream<'_> {
        FrameStream {
            client: self,
            done: false,
        }
    }
}

/// [`FrameClient::next`].
#[must_use = "futures do nothing unless polled"]
pub struct NextFrame<'a> {
    client: &'a FrameClient,
}

impl Future for NextFrame<'_> {
    type Output = RecvOutcome<FrameLease>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.client.poll_next(cx)
    }
}

/// [`FrameClient::stream`].
#[must_use = "streams do nothing unless polled"]
pub struct FrameStream<'a> {
    client: &'a FrameClient,
    done: bool,
}

impl futures_core::Stream for FrameStream<'_> {
    type Item = FrameLease;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<FrameLease>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.client.poll_next(cx) {
            Poll::Ready(RecvOutcome::Data(frame)) => Poll::Ready(Some(frame)),
            Poll::Ready(_) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
