//! What a [`FrameGrouper`](super::FrameGrouper) takes frames from: [`GroupSource`].

use std::os::fd::{AsFd, BorrowedFd};
use std::task::{Context, Poll, Waker};

use styx_core::prelude::*;

use crate::capture_api::CaptureHandle;
use crate::ipc::{ClientEvent, FrameClient};
use crate::session::MediaPipeline;

/// What a [`GroupSource`] has now.
// Moved once from the source to the grouper: boxing the frame would allocate per frame.
#[allow(clippy::large_enum_variant)]
pub enum SourceEvent {
    Frame(FrameLease),
    /// (Re)connected: frames follow.
    Connected,
    /// The connection is gone; a reconnecting source sends `Connected` again later.
    Disconnected,
    /// Nothing now: the source wakes the waker (or its descriptor) when that changes.
    Empty,
    /// Gone for good.
    Closed,
}

/// A camera feeding a [`FrameGrouper`](super::FrameGrouper): a [`CaptureHandle`], a
/// [`MediaPipeline`], a [`FrameClient`] of a camera service, a [`BoundedRx`] of frames, or
/// anything implementing it. The grouper never blocks in a source.
pub trait GroupSource: Send {
    /// The next event without waiting. When it returns `Empty` the source arranges to be woken:
    /// through `waker` (in-process queues register it), or by its descriptor ([`Self::fd`])
    /// becoming readable.
    fn poll_event(&mut self, waker: &Waker) -> SourceEvent;

    /// A descriptor readable when [`Self::poll_event`] has something, for sources woken that
    /// way (it must stay the same while the source is in a grouper).
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }
}

impl std::fmt::Debug for SourceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(frame) => write!(f, "Frame({})", frame.meta().timestamp),
            Self::Connected => f.write_str("Connected"),
            Self::Disconnected => f.write_str("Disconnected"),
            Self::Empty => f.write_str("Empty"),
            Self::Closed => f.write_str("Closed"),
        }
    }
}

fn from_poll(poll: Poll<RecvOutcome<FrameLease>>) -> SourceEvent {
    match poll {
        Poll::Ready(RecvOutcome::Data(frame)) => SourceEvent::Frame(frame),
        Poll::Ready(RecvOutcome::Closed) => SourceEvent::Closed,
        Poll::Ready(RecvOutcome::Empty) | Poll::Pending => SourceEvent::Empty,
    }
}

impl GroupSource for CaptureHandle {
    fn poll_event(&mut self, waker: &Waker) -> SourceEvent {
        from_poll(self.poll_recv(&mut Context::from_waker(waker)))
    }
}

/// Frames come processed (decode, transforms, hooks run on the grouper's thread).
impl GroupSource for MediaPipeline {
    fn poll_event(&mut self, waker: &Waker) -> SourceEvent {
        from_poll(self.poll_next(&mut Context::from_waker(waker)))
    }
}

/// Frames from any queue: synthetic cameras, a capture's frames forwarded by another thread.
impl GroupSource for BoundedRx<FrameLease> {
    fn poll_event(&mut self, waker: &Waker) -> SourceEvent {
        from_poll(self.poll_recv(&mut Context::from_waker(waker)))
    }
}

/// A camera service client; a [reconnecting](FrameClient::reconnecting) one reports its
/// `Connected` / `Disconnected` changes, and the grouper stops waiting for it while it is away.
impl GroupSource for FrameClient {
    fn poll_event(&mut self, _waker: &Waker) -> SourceEvent {
        match self.try_client_event() {
            RecvOutcome::Data(ClientEvent::Data(frame)) => SourceEvent::Frame(frame),
            RecvOutcome::Data(ClientEvent::Connected { .. }) => SourceEvent::Connected,
            RecvOutcome::Data(ClientEvent::Disconnected { .. }) => SourceEvent::Disconnected,
            RecvOutcome::Empty => SourceEvent::Empty,
            RecvOutcome::Closed => SourceEvent::Closed,
        }
    }

    fn fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.as_fd())
    }
}

impl<S: GroupSource + ?Sized> GroupSource for Box<S> {
    fn poll_event(&mut self, waker: &Waker) -> SourceEvent {
        (**self).poll_event(waker)
    }

    fn fd(&self) -> Option<BorrowedFd<'_>> {
        (**self).fd()
    }
}
