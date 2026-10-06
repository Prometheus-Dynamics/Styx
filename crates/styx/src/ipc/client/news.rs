//! A client's connection changes, in order with what it receives: [`ClientEvent`].
//!
//! The client's [`Dialer`](super::dial::Dialer) queues each change (connected, disconnected)
//! when the client's own calls notice it, and arms the client's timer so its descriptor (and a
//! waiting task's waker) wakes; [`FrameClient::try_client_event`] and
//! [`ControlClient::try_client_event`] hand them out before anything received after them.
//!
//! [`FrameClient::try_client_event`]: super::FrameClient::try_client_event
//! [`ControlClient::try_client_event`]: super::ControlClient::try_client_event

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use styx_core::prelude::*;

use super::PollSet;
use crate::ipc::IpcError;

/// What a client's [`try_client_event`](super::FrameClient::try_client_event) returns: a
/// connection change, or what the client receives (a [`FrameLease`] for a
/// [`FrameClient`](super::FrameClient), a [`ControlEvent`](crate::ipc::ControlEvent) for a
/// [`ControlClient`](super::ControlClient)), in the order they happened.
///
/// Each transition is reported once: `Connected` for the first connection (also of a client
/// whose blocking `connect`/`request` already returned connected: it is the first event) and
/// for each reconnection, `Disconnected` when the connection is lost, or when a client that
/// does not reconnect gives up connecting. What arrived on a connection comes before its
/// `Disconnected`; what arrives on the next one after its `Connected`.
#[derive(Debug)]
pub enum ClientEvent<T> {
    /// The client is connected: its first connection (`reconnects: 0`) or a reconnection
    /// (counted as [`reconnects()`](super::FrameClient::reconnects) counts them). A frame
    /// client's request was accepted again; a control client subscribed again. Settings to
    /// keep across service restarts are applied again here.
    Connected {
        /// Reconnections so far, this one included (0: the first connection).
        reconnects: u64,
    },
    /// The connection is gone (`error`: why; a `ConnectionReset` I/O error when the service
    /// closed it), or a client that does not reconnect gave up before connecting. A
    /// reconnecting client tries again; one that does not returns `Closed` next.
    Disconnected {
        /// Why.
        error: IpcError,
    },
    /// What the client receives: a frame, a control change.
    Data(T),
}

/// A connection change, queued for [`ClientEvent`].
pub(super) enum Change {
    Connected(u64),
    Disconnected(IpcError),
}

impl<T> From<Change> for ClientEvent<T> {
    fn from(change: Change) -> Self {
        match change {
            Change::Connected(reconnects) => ClientEvent::Connected { reconnects },
            Change::Disconnected(error) => ClientEvent::Disconnected { error },
        }
    }
}

/// Most changes kept unread: beyond, the oldest pair (a connection and its loss, or a loss and
/// the next connection) is dropped, so what is read still alternates and ends in the client's
/// state now. A client reading its events never gets near: changes are at least 100 ms apart
/// (the reconnection backoff). It bounds the queue of a client read only with `try_next` or
/// `try_event`, which leave changes alone.
const MAX_CHANGES: usize = 16;

/// The changes not read yet.
#[derive(Default)]
pub(super) struct News {
    queue: VecDeque<Change>,
}

impl News {
    /// Queue `change`; the client's descriptor wakes.
    pub(super) fn push(&mut self, poll: &PollSet, change: Change) {
        if self.queue.len() >= MAX_CHANGES {
            self.queue.drain(..2);
        }
        self.queue.push_back(change);
        poll.wake_at(Some(Instant::now()));
    }

    pub(super) fn pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// The oldest change; the descriptor stays readable while more wait.
    pub(super) fn pop(&mut self, poll: &PollSet) -> Option<Change> {
        let change = self.queue.pop_front()?;
        if !self.queue.is_empty() {
            poll.wake_at(Some(Instant::now()));
        }
        Some(change)
    }
}

/// A client whose events are awaited: [`NextClientEvent`], [`ClientEventStream`].
pub(super) trait EventSource<T> {
    fn poll_client_event(&self, cx: &mut Context<'_>) -> Poll<RecvOutcome<ClientEvent<T>>>;
}

/// [`FrameClient::next_client_event`](super::FrameClient::next_client_event),
/// [`ControlClient::next_client_event`](super::ControlClient::next_client_event).
#[must_use = "futures do nothing unless polled"]
pub struct NextClientEvent<'a, T> {
    pub(super) client: &'a (dyn EventSource<T> + Sync),
}

impl<T> Future for NextClientEvent<'_, T> {
    type Output = RecvOutcome<ClientEvent<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.client.poll_client_event(cx)
    }
}

/// [`FrameClient::client_events`](super::FrameClient::client_events),
/// [`ControlClient::client_events`](super::ControlClient::client_events): a
/// [`Stream`](futures_core::Stream) ending when the client is closed (never for a reconnecting
/// one).
#[must_use = "streams do nothing unless polled"]
pub struct ClientEventStream<'a, T> {
    pub(super) client: &'a (dyn EventSource<T> + Sync),
    pub(super) done: bool,
}

impl<T> futures_core::Stream for ClientEventStream<'_, T> {
    type Item = ClientEvent<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.client.poll_client_event(cx) {
            Poll::Ready(RecvOutcome::Data(event)) => Poll::Ready(Some(event)),
            Poll::Ready(_) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Poll `try_event` until it has something, waiting on the client's descriptor through
/// styx-graph's reactor in between (`Closed` when there is no reactor).
pub(super) fn poll_with<T>(
    poll: &PollSet,
    cx: &mut Context<'_>,
    mut try_event: impl FnMut() -> RecvOutcome<T>,
) -> Poll<RecvOutcome<T>> {
    loop {
        match try_event() {
            RecvOutcome::Empty => {}
            outcome => return Poll::Ready(outcome),
        }
        match poll.reactor().map(|reactor| reactor.poll_read_ready(cx)) {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(Ok(_))) => {}
            Ok(Poll::Ready(Err(_))) | Err(_) => return Poll::Ready(RecvOutcome::Closed),
        }
    }
}
