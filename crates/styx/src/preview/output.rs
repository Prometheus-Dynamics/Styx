//! The latest encoded frame and the subscribers waiting for the next one.
//!
//! One slot, overwritten by each new frame: a subscriber gets the newest frame it has not
//! seen, never a queue, so a slow viewer skips frames instead of falling behind. Waiting works
//! blocking ([`PreviewSubscriber::recv`]) and on any executor ([`PreviewSubscriber::next`],
//! `Stream`), woken by the encoder thread.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::PreviewFrame;

#[derive(Default)]
struct Slot {
    latest: Option<PreviewFrame>,
    closed: bool,
    /// Wakers of subscribers waiting asynchronously, by subscriber.
    wakers: HashMap<u64, Waker>,
}

/// Shared between the encoder thread, the [`Preview`](super::Preview) handle and subscribers.
#[derive(Default)]
pub(super) struct Output {
    slot: Mutex<Slot>,
    ready: Condvar,
    /// Subscribers alive now (the encoder idles without them, unless told otherwise).
    pub(super) subscribers: AtomicU64,
    next_subscriber: AtomicU64,
    /// Signalled when the subscriber count changes, for the encoder thread.
    pub(super) watchers: Condvar,
    pub(super) watch_lock: Mutex<()>,
}

impl Output {
    /// Publish `frame` as the latest and wake every waiting subscriber.
    pub(super) fn publish(&self, frame: PreviewFrame) {
        let wakers: Vec<Waker> = {
            let mut slot = self.slot.lock();
            slot.latest = Some(frame);
            slot.wakers.drain().map(|(_, w)| w).collect()
        };
        self.ready.notify_all();
        wakers.into_iter().for_each(Waker::wake);
    }

    /// No more frames: subscribers end after the latest.
    pub(super) fn close(&self) {
        let wakers: Vec<Waker> = {
            let mut slot = self.slot.lock();
            slot.closed = true;
            slot.wakers.drain().map(|(_, w)| w).collect()
        };
        self.ready.notify_all();
        wakers.into_iter().for_each(Waker::wake);
        self.watchers.notify_all();
    }

    pub(super) fn latest(&self) -> Option<PreviewFrame> {
        self.slot.lock().latest.clone()
    }

    pub(super) fn subscribe(self: &Arc<Self>) -> PreviewSubscriber {
        self.subscribers.fetch_add(1, Ordering::AcqRel);
        self.notify_watchers();
        PreviewSubscriber {
            output: self.clone(),
            id: self.next_subscriber.fetch_add(1, Ordering::Relaxed),
            seen: 0,
        }
    }

    fn notify_watchers(&self) {
        let _guard = self.watch_lock.lock();
        self.watchers.notify_all();
    }

    /// Wait up to `timeout` for the subscriber count to change.
    pub(super) fn wait_watchers(&self, timeout: Duration) {
        let mut guard = self.watch_lock.lock();
        self.watchers.wait_for(&mut guard, timeout);
    }

    /// The latest frame newer than `seen`, or `None`; `Err(())` when closed with nothing newer.
    fn newer(slot: &Slot, seen: u64) -> Result<Option<PreviewFrame>, ()> {
        match &slot.latest {
            Some(frame) if frame.sequence > seen => Ok(Some(frame.clone())),
            _ if slot.closed => Err(()),
            _ => Ok(None),
        }
    }
}

/// Encoded preview frames for one viewer: each call returns the newest frame it has not seen
/// yet, waiting for one if needed. Never queues: a viewer slower than the preview gets the
/// latest frame and skips the ones between (see [`PreviewFrame::sequence`]).
///
/// Blocking: [`PreviewSubscriber::recv`]. Async, on any executor: [`PreviewSubscriber::next`]
/// or the `futures_core::Stream` impl (it ends when the preview stops). While at least one
/// subscriber lives, the preview encodes (a preview with none idles by default; see
/// [`PreviewConfig::encode_unwatched`](super::PreviewConfig::encode_unwatched)).
pub struct PreviewSubscriber {
    output: Arc<Output>,
    id: u64,
    seen: u64,
}

impl std::fmt::Debug for PreviewSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewSubscriber")
            .field("id", &self.id)
            .field("seen", &self.seen)
            .finish()
    }
}

impl PreviewSubscriber {
    /// The newest frame not seen yet, without waiting.
    pub fn try_recv(&mut self) -> Option<PreviewFrame> {
        let frame = Output::newer(&self.output.slot.lock(), self.seen).ok()??;
        self.seen = frame.sequence;
        Some(frame)
    }

    /// The newest frame not seen yet, waiting up to `timeout` for one. `None` on timeout or
    /// when the preview has stopped.
    pub fn recv(&mut self, timeout: Duration) -> Option<PreviewFrame> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.output.slot.lock();
        loop {
            match Output::newer(&slot, self.seen) {
                Ok(Some(frame)) => {
                    self.seen = frame.sequence;
                    return Some(frame);
                }
                Err(()) => return None,
                Ok(None) => {}
            }
            if self
                .output
                .ready
                .wait_until(&mut slot, deadline)
                .timed_out()
            {
                return None;
            }
        }
    }

    /// Poll for the newest frame not seen yet (`Ready(None)`: the preview stopped).
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<PreviewFrame>> {
        let mut slot = self.output.slot.lock();
        match Output::newer(&slot, self.seen) {
            Ok(Some(frame)) => {
                slot.wakers.remove(&self.id);
                self.seen = frame.sequence;
                Poll::Ready(Some(frame))
            }
            Err(()) => Poll::Ready(None),
            Ok(None) => {
                slot.wakers.insert(self.id, cx.waker().clone());
                Poll::Pending
            }
        }
    }

    /// The newest frame not seen yet, awaited on any executor (`None`: the preview stopped).
    // A future, as `StreamExt::next` gives: the subscriber is a `Stream`, not an `Iterator`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> NextPreviewFrame<'_> {
        NextPreviewFrame(self)
    }

    /// The sequence number of the last frame this subscriber got (0: none yet).
    pub fn seen(&self) -> u64 {
        self.seen
    }
}

impl Clone for PreviewSubscriber {
    /// Another subscriber, starting from the frames this one has seen.
    fn clone(&self) -> Self {
        let mut other = self.output.subscribe();
        other.seen = self.seen;
        other
    }
}

impl Drop for PreviewSubscriber {
    fn drop(&mut self) {
        self.output.slot.lock().wakers.remove(&self.id);
        self.output.subscribers.fetch_sub(1, Ordering::AcqRel);
        self.output.notify_watchers();
    }
}

impl futures_core::Stream for PreviewSubscriber {
    type Item = PreviewFrame;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<PreviewFrame>> {
        self.get_mut().poll_recv(cx)
    }
}

/// [`PreviewSubscriber::next`]'s future.
#[derive(Debug)]
pub struct NextPreviewFrame<'a>(&'a mut PreviewSubscriber);

impl Future for NextPreviewFrame<'_> {
    type Output = Option<PreviewFrame>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().0.poll_recv(cx)
    }
}
