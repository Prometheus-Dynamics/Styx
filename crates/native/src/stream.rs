//! Frames as an async stream over the capture node, with blocking wrappers.
//!
//! The video node is registered with `styx-graph`'s reactor: waiting for a frame is waiting for
//! the node to be readable, on any executor (or none, with [`FrameStream::next_blocking`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_core::Stream;
use styx_graph::rt::{self, AsyncFd};
use styx_kernel::FourCc;
use styx_kernel::bus::StreamRequest;
use styx_kernel::v4l2::{BufType, BufferFlags, VideoDevice};
use styx_sensor::{RegisterBus, SensorPins};

use crate::buffers::{FrameHead, Lender, NativeFrame};
use crate::control::{FrameControls, SensorControl, lock};
use crate::embedded::EmbeddedCapture;
use crate::error::{KernelContext, NativeError, Result};

/// What the stream and the event thread need from the sensor side, without its bus types.
pub(crate) trait SensorSide: Send + Sync {
    /// Serves a bridge request (the acknowledgement result).
    fn serve(&self, req: &StreamRequest) -> std::result::Result<(), i32>;
    /// A frame started.
    fn frame_start(&self, seq: u64);
    /// The values that produced frame `seq`.
    fn applied(&self, seq: u64) -> Option<FrameControls>;
    /// Embedded data of frame `seq` (the raw buffer).
    fn report_embedded(&self, seq: u64, data: &[u8]);
}

impl<B, P> SensorSide for Mutex<SensorControl<B, P>>
where
    B: RegisterBus + Send,
    P: SensorPins + Send,
{
    fn serve(&self, req: &StreamRequest) -> std::result::Result<(), i32> {
        lock(self).serve(req)
    }

    fn frame_start(&self, seq: u64) {
        // A failed write shows up as a mismatch in the frame's values; nothing to return to.
        let _ = lock(self).frame_start(seq);
    }

    fn applied(&self, seq: u64) -> Option<FrameControls> {
        lock(self).applied(seq)
    }

    fn report_embedded(&self, seq: u64, data: &[u8]) {
        let _ = lock(self).report_embedded(seq, data);
    }
}

/// Stream statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Frames delivered.
    pub frames: u64,
    /// Frames missing according to sequence numbers.
    pub dropped: u64,
    /// Frames the receiver flagged as corrupted.
    pub errors: u64,
}

/// State shared by a stream, its frames and the camera.
pub(crate) struct StreamShared {
    pub(crate) fd: Arc<AsyncFd<Arc<VideoDevice>>>,
    pub(crate) lender: Arc<Lender>,
    pub(crate) sensor: Arc<dyn SensorSide>,
    pub(crate) embedded: Option<Arc<EmbeddedCapture>>,
    pub(crate) buf_type: BufType,
    /// Frame starts come from `FRAME_SYNC` events; without them the dequeue drives the
    /// schedule.
    pub(crate) frame_sync: bool,
    pub(crate) fourcc: FourCc,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
    pub(crate) started: Instant,
    pub(crate) first_frame: OnceLock<Instant>,
    pub(crate) frames: AtomicU64,
    pub(crate) dropped: AtomicU64,
    pub(crate) errors: AtomicU64,
    pub(crate) last_sequence: AtomicU64,
    pub(crate) disconnected: AtomicBool,
}

impl StreamShared {
    fn streaming(&self) -> bool {
        self.lender.streaming.load(Ordering::Acquire)
    }

    /// Dequeues a finished buffer if there is one.
    fn try_dequeue(self: &Arc<Self>) -> Result<Option<NativeFrame>> {
        let video = self.fd.get_ref();
        let Some(buf) = video
            .dequeue(self.buf_type, self.lender.buffers.memory())
            .step("VIDIOC_DQBUF")?
        else {
            return Ok(None);
        };
        let seq = u64::from(buf.sequence);
        if !self.frame_sync {
            // Dequeued after its end: the next frame is starting.
            self.sensor.frame_start(seq + 1);
        }
        let last = self.last_sequence.swap(seq + 1, Ordering::AcqRel);
        if last != 0 && seq + 1 > last + 1 {
            self.dropped.fetch_add(seq - last, Ordering::Relaxed);
        }
        let error = buf.flags.contains(BufferFlags::ERROR);
        if error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.frames.fetch_add(1, Ordering::Relaxed);
        let _ = self.first_frame.set(Instant::now());
        let head = FrameHead {
            sequence: buf.sequence,
            timestamp: buf.timestamp,
            bytes_used: buf.bytes_used(),
            error,
            fourcc: self.fourcc,
            width: self.width,
            height: self.height,
            stride: self.stride,
        };
        if let Some(e) = &self.embedded {
            e.drain();
        }
        let controls = self.sensor.applied(seq);
        Ok(Some(NativeFrame::new(
            buf.index,
            Arc::clone(&self.lender),
            head,
            controls,
        )))
    }

    /// The next frame; `None` once the stream stopped.
    async fn next_frame(self: Arc<Self>) -> Option<Result<NativeFrame>> {
        loop {
            if !self.streaming() {
                return None;
            }
            match self.try_dequeue() {
                Ok(Some(f)) => return Some(Ok(f)),
                Ok(None) => {}
                Err(e) if !self.streaming() => {
                    drop(e);
                    return None;
                }
                Err(e) if e.is_disconnect() => {
                    self.disconnected.store(true, Ordering::Release);
                    return Some(Err(NativeError::Disconnected));
                }
                Err(e) => return Some(Err(e)),
            }
            match self.fd.readable().await {
                Ok(ready) if ready.is_hangup() => {
                    self.disconnected.store(true, Ordering::Release);
                    return Some(Err(NativeError::Disconnected));
                }
                // An error with the stream stopped ends it; while streaming the next dequeue
                // reports what happened.
                Ok(_) => {}
                Err(e) => return Some(Err(NativeError::kernel("wait for a frame", e))),
            }
        }
    }
}

type NextFrame = Pin<Box<dyn Future<Output = Option<Result<NativeFrame>>> + Send>>;

/// Frames of a started camera. Ends when the camera stops; yields
/// [`NativeError::Disconnected`] and ends if the device goes away.
pub struct FrameStream {
    shared: Arc<StreamShared>,
    pending: Option<NextFrame>,
    ended: bool,
}

impl FrameStream {
    pub(crate) fn new(shared: Arc<StreamShared>) -> Self {
        Self {
            shared,
            pending: None,
            ended: false,
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
        self.shared.started
    }

    /// When the first frame was dequeued.
    pub fn first_frame(&self) -> Option<Instant> {
        self.shared.first_frame.get().copied()
    }

    /// Frames delivered, dropped and flagged so far.
    pub fn stats(&self) -> StreamStats {
        StreamStats {
            frames: self.shared.frames.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            errors: self.shared.errors.load(Ordering::Relaxed),
        }
    }

    /// Pixel format, size and stride of the frames.
    pub fn format(&self) -> (FourCc, u32, u32, u32) {
        let s = &self.shared;
        (s.fourcc, s.width, s.height, s.stride)
    }

    /// Frames currently held by the application.
    pub fn outstanding(&self) -> usize {
        self.shared.lender.buffers.outstanding()
    }

    /// Buffers in the queue.
    pub fn buffer_count(&self) -> usize {
        self.shared.lender.buffers.count()
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
        if self.ended {
            return Poll::Ready(None);
        }
        let this = &mut *self;
        let fut = this
            .pending
            .get_or_insert_with(|| Box::pin(Arc::clone(&this.shared).next_frame()));
        match fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(item) => {
                this.pending = None;
                match &item {
                    None | Some(Err(NativeError::Disconnected)) => this.ended = true,
                    _ => {}
                }
                Poll::Ready(item)
            }
        }
    }
}
