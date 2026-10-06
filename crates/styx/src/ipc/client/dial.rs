//! Opening a connection to a camera service without blocking: a non-blocking connect, the
//! client's first message, then the service's answer whenever it comes. Each step returns at
//! once; the client's [`PollSet`] wakes for the answer (the socket is watched) and for the next
//! attempt (its timer), so a `poll` loop or an executor drives it. Frame clients
//! ([`FrameClient`](super::FrameClient)) and control clients
//! ([`ControlClient`](super::ControlClient)) connect, and reconnect, through it.

use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::{Duration, Instant};

use super::PollSet;
use crate::ipc::wire::{self, ServerMessage};
use crate::ipc::{IpcError, socket};

/// Backoff between connection attempts.
pub(super) const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(2);

/// Where a client is in connecting: an attempt under way, when the next one is due, and, until
/// its first connection, when a client that does not reconnect gives up.
pub(super) struct Dialer {
    /// An attempt's socket, waiting for the service's answer until the deadline.
    pub(super) pending: Option<(OwnedFd, Instant)>,
    pub(super) next_attempt: Instant,
    backoff: Duration,
    /// Until the first connection: the deadline of a client that does not reconnect (`None`
    /// for one that does: it tries until it connects).
    first: Option<Option<Instant>>,
    /// The client gave up (it does not reconnect, and the first connection failed).
    gave_up: bool,
    /// Why the last attempt failed; cleared on connecting.
    error: Option<IpcError>,
}

/// How an attempt ended when it was not answered yet.
pub(super) enum Step<T> {
    /// Connected: the socket, and what the service answered.
    Connected(OwnedFd, T),
    /// Nothing yet (an attempt under way, or the next one not due).
    Waiting,
    /// A client that does not reconnect gave up ([`Dialer::error`] says why).
    GaveUp,
}

impl Dialer {
    /// For a client that is connected already.
    pub(super) fn connected() -> Self {
        Self {
            pending: None,
            next_attempt: Instant::now(),
            backoff: RETRY_MIN,
            first: None,
            gave_up: false,
            error: None,
        }
    }

    /// For a client that connects in the background: until `deadline` (`None`: until it does).
    pub(super) fn connecting(deadline: Option<Instant>) -> Self {
        Self {
            first: Some(deadline),
            ..Self::connected()
        }
    }

    /// Before the first connection, and not given up: connecting counts as reconnecting.
    pub(super) fn first_pending(&self) -> bool {
        self.first.is_some()
    }

    /// Why the last attempt failed (a copy).
    pub(super) fn error(&self) -> Option<IpcError> {
        self.error.as_ref().map(copy_error)
    }

    /// A connection was made: no backoff for the next loss. Whether it was the first.
    pub(super) fn succeeded(&mut self) -> bool {
        self.backoff = RETRY_MIN;
        self.error = None;
        self.first.take().is_some()
    }

    /// Schedule the next attempt after `err`; a client that does not reconnect gives up when
    /// the service refused it or its first deadline has passed.
    pub(super) fn failed(&mut self, poll: &PollSet, err: IpcError, reconnect: bool) {
        crate::trace::debug!(error = %err, "camera service not there (yet)");
        if let Some((socket, _)) = self.pending.take() {
            poll.unwatch(&socket);
        }
        let now = Instant::now();
        self.next_attempt = now + self.backoff;
        self.backoff = (self.backoff * 2).min(RETRY_MAX);
        let refused = matches!(
            err,
            IpcError::Rejected(_) | IpcError::ControlRefused(_) | IpcError::Malformed(_)
        );
        self.error = Some(err);
        let deadline = self.first.flatten();
        if !reconnect && (refused || deadline.is_none_or(|d| now >= d)) {
            self.first = None;
            self.gave_up = true;
            // Readable from now on: whoever polls learns it is closed.
            poll.wake_at(Some(now));
            return;
        }
        let at = match deadline {
            Some(d) if !reconnect => self.next_attempt.min(d),
            _ => self.next_attempt,
        };
        poll.wake_at(Some(at));
    }

    /// One step, without blocking: take the service's answer to the attempt under way, or
    /// start an attempt when one is due (connect to `path`, send `hello`). `answer` reads a
    /// message from the service: `None` to skip it (a frame or event before the answer).
    pub(super) fn step<T>(
        &mut self,
        poll: &PollSet,
        path: &Path,
        timeout: Duration,
        reconnect: bool,
        hello: impl FnOnce() -> Vec<u8>,
        answer: impl Fn(ServerMessage) -> Option<Result<T, IpcError>>,
    ) -> Step<T> {
        if self.gave_up {
            return Step::GaveUp;
        }
        if let Some((pending, deadline)) = self.pending.take() {
            let outcome = match socket::recv(&pending, Duration::ZERO) {
                Ok(socket::Received::Message(bytes, _)) => match wire::decode_server(&bytes) {
                    Ok(message) => answer(message),
                    Err(err) => Some(Err(err)),
                },
                Ok(socket::Received::Nothing) if Instant::now() < deadline => None,
                Ok(socket::Received::Nothing) => {
                    Some(Err(io::Error::from(io::ErrorKind::TimedOut).into()))
                }
                Ok(socket::Received::Closed) => {
                    Some(Err(io::Error::from(io::ErrorKind::ConnectionReset).into()))
                }
                Err(err) => Some(Err(err.into())),
            };
            return match outcome {
                None => {
                    self.pending = Some((pending, deadline));
                    poll.wake_at(Some(deadline));
                    Step::Waiting
                }
                Some(Ok(answered)) => Step::Connected(pending, answered),
                Some(Err(err)) => {
                    self.pending = Some((pending, deadline));
                    self.failed(poll, err, reconnect);
                    if self.gave_up {
                        Step::GaveUp
                    } else {
                        Step::Waiting
                    }
                }
            };
        }
        if !reconnect
            && let Some(Some(first)) = self.first
            && Instant::now() >= first
        {
            let err = self
                .error
                .take()
                .unwrap_or_else(|| IpcError::Io(io::ErrorKind::TimedOut.into()));
            self.failed(poll, err, reconnect);
            return Step::GaveUp;
        }
        if Instant::now() < self.next_attempt {
            poll.wake_at(Some(self.next_attempt));
            return Step::Waiting;
        }
        let started = socket::Connecting::new(path).and_then(|connecting| {
            if !connecting.attempt()? {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let socket = connecting.into_socket();
            socket::send(&socket, &hello(), &[])?;
            Ok(socket)
        });
        match started {
            Ok(socket) => {
                let mut deadline = Instant::now() + timeout;
                if !reconnect && let Some(Some(first)) = self.first {
                    deadline = deadline.min(first.max(Instant::now()));
                }
                poll.watch(&socket);
                poll.wake_at(Some(deadline));
                self.pending = Some((socket, deadline));
                Step::Waiting
            }
            Err(err) => {
                self.failed(poll, err.into(), reconnect);
                if self.gave_up {
                    Step::GaveUp
                } else {
                    Step::Waiting
                }
            }
        }
    }
}

/// A copy of `err` (I/O errors keep their kind and message).
pub(super) fn copy_error(err: &IpcError) -> IpcError {
    match err {
        IpcError::Io(err) => IpcError::Io(io::Error::new(err.kind(), err.to_string())),
        IpcError::Malformed(what) => IpcError::Malformed(what),
        IpcError::Rejected(why) => IpcError::Rejected(why.clone()),
        IpcError::ControlRefused(refusal) => IpcError::ControlRefused(refusal.clone()),
        other => IpcError::Io(io::Error::other(other.to_string())),
    }
}
