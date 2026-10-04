//! Frames as an async stream: the runtime's [`styx_runtime::FrameStream`] on the Linux
//! receiver, with blocking wrappers and the Linux frame type.
//!
//! Waiting for a frame registers the caller's waker with the receiver, whose event thread polls
//! the capture node and wakes it: any executor works, or none
//! ([`FrameStream::next_blocking`]).

use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_core::Stream;
use styx_graph::rt;
use styx_kernel::FourCc;

pub(crate) use styx_runtime::SensorSide;
pub use styx_runtime::StreamStats;

use crate::buffers::NativeFrame;
use crate::error::{NativeError, Result};
use crate::receiver::Linux;

/// Frames of a started camera, as an async [`Stream`] or with [`FrameStream::next_blocking`].
///
/// Ends (`None`) when the camera stops. A fault ends it after one error item:
/// [`NativeError::Disconnected`] when the bridge or the capture node goes away, a kernel error
/// when the receiver fails, the sensor stops answering, or too many frames in a row arrive
/// corrupted. The camera then still has to be stopped (or dropped), which puts the sensor in
/// standby and powers it down.
///
/// Works on any executor (tokio, smol, a hand-written loop) or none: a waiting stream is woken
/// through its [`std::task::Waker`] by the camera's event thread, which polls the capture node.
/// Dropping a pending `next()` future, or the stream itself, at any point loses no frame and
/// leaks nothing: a frame is only taken from the queue inside a poll that returns it.
pub struct FrameStream {
    inner: styx_runtime::FrameStream<Linux>,
    format: (FourCc, u32, u32, u32),
    started: Instant,
    first_frame: OnceLock<Instant>,
}

impl FrameStream {
    pub(crate) fn new(
        inner: styx_runtime::FrameStream<Linux>,
        format: (FourCc, u32, u32, u32),
        started: Instant,
    ) -> Self {
        Self {
            inner,
            format,
            started,
            first_frame: OnceLock::new(),
        }
    }

    /// Waits up to `timeout` for the next frame, blocking this thread. `Ok(None)` at the end of
    /// the stream, [`NativeError::Timeout`] when no frame came in time.
    pub fn next_blocking(&mut self, timeout: Duration) -> Result<Option<NativeFrame>> {
        match rt::block_on(rt::timeout(timeout, rt::next(self))) {
            Ok(Some(r)) => r.map(Some),
            Ok(None) => Ok(None),
            Err(_) => Err(NativeError::Timeout),
        }
    }

    /// When the stream started (just before `VIDIOC_STREAMON`).
    pub fn started(&self) -> Instant {
        self.started
    }

    /// When the first frame was dequeued.
    pub fn first_frame(&self) -> Option<Instant> {
        self.first_frame.get().copied()
    }

    /// Frames delivered, dropped and flagged so far.
    pub fn stats(&self) -> StreamStats {
        self.inner.stats()
    }

    /// Pixel format, size and stride of the frames.
    pub fn format(&self) -> (FourCc, u32, u32, u32) {
        self.format
    }

    /// Frames currently held by the application.
    pub fn outstanding(&self) -> usize {
        self.inner.outstanding()
    }

    /// Buffers in the queue.
    pub fn buffer_count(&self) -> usize {
        self.inner.buffer_count()
    }

    /// Whether frame starts are currently inferred from dequeued frames because the
    /// receiver's frame-start events stopped (or never came).
    pub fn frame_sync_fallback(&self) -> bool {
        self.inner.frame_sync_fallback()
    }

    /// Whether the stream ended because the device went away.
    pub fn is_disconnected(&self) -> bool {
        self.inner.is_disconnected()
    }
}

impl std::fmt::Debug for FrameStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameStream")
            .field("format", &self.format())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl Stream for FrameStream {
    type Item = Result<NativeFrame>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let item = std::task::ready!(this.inner.poll_frame(cx));
        Poll::Ready(item.map(|r| match r {
            Ok(frame) => {
                let now = Instant::now();
                let _ = this.first_frame.set(now);
                let (fourcc, width, height, stride) = this.format;
                Ok(NativeFrame::new(frame, now, fourcc, width, height, stride))
            }
            Err(e) => Err(e.into()),
        }))
    }
}
