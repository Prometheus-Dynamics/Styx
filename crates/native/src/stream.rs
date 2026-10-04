//! Frames as an async stream over the capture node, with blocking wrappers.
//!
//! The video node is registered with `styx-graph`'s reactor: waiting for a frame is waiting for
//! the node to be readable, on any executor (or none, with [`FrameStream::next_blocking`]).

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_core::Stream;
use styx_graph::rt;
use styx_kernel::FourCc;
use styx_kernel::bus::StreamRequest;
use styx_kernel::v4l2::BufferFlags;
use styx_runtime::styx_hal::ErrorKind;
use styx_sensor::{RegisterBus, SensorPins};

use crate::buffers::{FrameHead, Lender, NativeFrame};
use crate::control::{BridgeServe, FrameControls, SensorControl, lock};
use crate::device::CaptureDevice;
use crate::embedded::EmbeddedCapture;
use crate::error::{KernelContext, NativeError, Result};
use crate::events::FrameNotify;
use crate::health::{Fault, Health, fault_error};

/// What the stream and the event thread need from the sensor side, without its bus types.
pub(crate) trait SensorSide: Send + Sync {
    /// Serves a bridge request (the acknowledgement result: an errno and why).
    fn serve(&self, req: &StreamRequest) -> std::result::Result<(), (i32, String)>;
    /// A kernel driver's sensor is about to be started by the receiver: writes frame 0's
    /// values and starts the control schedule.
    fn start_streaming(&self) -> std::result::Result<(), String>;
    /// A frame started (at `at` on `CLOCK_MONOTONIC`, when known): writes what is due.
    fn frame_start(&self, seq: u64, at: Option<Duration>) -> std::result::Result<(), String>;
    /// The values that produced frame `seq`.
    fn applied(&self, seq: u64) -> Option<FrameControls>;
    /// Requested values wait for a coming frame start.
    fn writes_pending(&self) -> bool;
    /// Embedded data of frame `seq` (the raw buffer).
    fn report_embedded(&self, seq: u64, data: &[u8]);
    /// Puts the sensor back in standby if it streams.
    fn standby(&self);
    /// Standby and power down, whatever the state.
    fn shut_down(&self) -> Result<()>;
}

impl<B, P> SensorSide for Mutex<SensorControl<B, P>>
where
    B: RegisterBus + Send,
    P: SensorPins + Send,
{
    fn serve(&self, req: &StreamRequest) -> std::result::Result<(), (i32, String)> {
        BridgeServe::serve_detailed(&mut *lock(self), req)
    }

    fn start_streaming(&self) -> std::result::Result<(), String> {
        lock(self).start_streaming().map_err(|e| e.to_string())
    }

    fn frame_start(&self, seq: u64, at: Option<Duration>) -> std::result::Result<(), String> {
        lock(self)
            .frame_start_at(seq, at)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn applied(&self, seq: u64) -> Option<FrameControls> {
        lock(self).applied(seq)
    }

    fn writes_pending(&self) -> bool {
        lock(self).writes_pending()
    }

    fn report_embedded(&self, seq: u64, data: &[u8]) {
        let _ = lock(self).report_embedded(seq, data);
    }

    fn standby(&self) {
        lock(self).standby();
    }

    fn shut_down(&self) -> Result<()> {
        Ok(lock(self).shut_down()?)
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

/// Frames dequeued without a frame-start event after which frame starts are taken from the
/// dequeues instead (the receiver stopped sending them, or never did).
const SYNC_MISSING_FRAMES: u32 = 3;

/// State shared by a stream, its frames and the camera.
pub(crate) struct StreamShared {
    pub(crate) video: Arc<dyn CaptureDevice>,
    /// Wakes a waiting stream (through the event thread, which polls the node).
    pub(crate) notify: Arc<FrameNotify>,
    pub(crate) lender: Arc<Lender>,
    pub(crate) sensor: Arc<dyn SensorSide>,
    pub(crate) embedded: Option<Arc<EmbeddedCapture>>,
    pub(crate) health: Arc<Health>,
    /// Frame starts come from `FRAME_SYNC` events; without them the dequeue drives the
    /// schedule.
    pub(crate) frame_sync: bool,
    /// Consecutive corrupted frames that end the stream (0: never).
    pub(crate) max_error_frames: u32,
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
    /// Frame-start events seen at the last dequeue, and dequeues since the count moved.
    pub(crate) syncs_seen: AtomicU64,
    pub(crate) frames_without_sync: AtomicU32,
    /// Frame starts are taken from dequeues because the events stopped.
    pub(crate) sync_fallback: AtomicBool,
    pub(crate) consecutive_errors: AtomicU32,
}

impl StreamShared {
    pub(crate) fn new(
        video: Arc<dyn CaptureDevice>,
        notify: Arc<FrameNotify>,
        lender: Arc<Lender>,
        sensor: Arc<dyn SensorSide>,
        health: Arc<Health>,
        format: (FourCc, u32, u32, u32),
    ) -> Self {
        let (fourcc, width, height, stride) = format;
        Self {
            video,
            notify,
            lender,
            sensor,
            embedded: None,
            health,
            frame_sync: false,
            max_error_frames: 0,
            fourcc,
            width,
            height,
            stride,
            started: Instant::now(),
            first_frame: OnceLock::new(),
            frames: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_sequence: AtomicU64::new(0),
            disconnected: AtomicBool::new(false),
            syncs_seen: AtomicU64::new(0),
            frames_without_sync: AtomicU32::new(0),
            sync_fallback: AtomicBool::new(false),
            consecutive_errors: AtomicU32::new(0),
        }
    }

    fn streaming(&self) -> bool {
        self.lender.buffers.is_live()
    }

    /// Whether this dequeue has to drive the control schedule: without frame-start events, or
    /// once they went missing for a few frames (until they come back).
    fn drive_from_dequeue(&self) -> bool {
        if !self.frame_sync {
            return true;
        }
        let syncs = self.health.frame_syncs.get();
        if self.syncs_seen.swap(syncs, Ordering::Relaxed) != syncs {
            self.frames_without_sync.store(0, Ordering::Relaxed);
            self.sync_fallback.store(false, Ordering::Relaxed);
            return false;
        }
        let n = self.frames_without_sync.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= SYNC_MISSING_FRAMES {
            self.sync_fallback.store(true, Ordering::Relaxed);
        }
        self.sync_fallback.load(Ordering::Relaxed)
    }

    /// Dequeues a finished buffer if there is one.
    fn try_dequeue(&self) -> Result<Option<NativeFrame>> {
        let video = &self.video;
        let Some(buf) = video
            .dequeue(self.lender.buffers.memory())
            .step("VIDIOC_DQBUF")?
        else {
            return Ok(None);
        };
        let seq = u64::from(buf.sequence);
        if self.drive_from_dequeue() {
            // Dequeued after its end: the next frame is starting.
            self.health
                .control_write(self.sensor.frame_start(seq + 1, None));
        }
        let last = self.last_sequence.swap(seq + 1, Ordering::AcqRel);
        if last != 0 && seq + 1 > last + 1 {
            self.dropped.fetch_add(seq - last, Ordering::Relaxed);
        }
        let error = buf.flags.contains(BufferFlags::ERROR);
        if error {
            self.errors.fetch_add(1, Ordering::Relaxed);
            let n = self.consecutive_errors.fetch_add(1, Ordering::Relaxed) + 1;
            if self.max_error_frames > 0 && n >= self.max_error_frames {
                self.health.fail(Fault::new(
                    ErrorKind::Corrupt,
                    format!("the receiver flagged {n} frames in a row as corrupted"),
                    Some(libc::EIO),
                ));
            }
        } else {
            self.consecutive_errors.store(0, Ordering::Relaxed);
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

    fn fault(&self) -> Option<NativeError> {
        let f = self.health.fault()?;
        if f.is_disconnect() {
            self.disconnected.store(true, Ordering::Release);
        }
        Some(fault_error(&f))
    }

    /// The next frame, `None` once the stream stopped, or `Pending` with `cx`'s waker
    /// registered. A frame is only taken from the queue when it is returned.
    fn poll_frame(&self, cx: &mut Context<'_>) -> Poll<Option<Result<NativeFrame>>> {
        if let Some(e) = self.fault() {
            return Poll::Ready(Some(Err(e)));
        }
        if !self.streaming() {
            return Poll::Ready(None);
        }
        // Register before looking, so a frame that arrives in between wakes us.
        self.notify.register(cx.waker());
        match self.try_dequeue() {
            Ok(Some(f)) => Poll::Ready(Some(Ok(f))),
            Ok(None) => Poll::Pending,
            Err(_) if !self.streaming() => Poll::Ready(None),
            Err(e) if e.is_disconnect() => {
                self.disconnected.store(true, Ordering::Release);
                Poll::Ready(Some(Err(NativeError::Disconnected)))
            }
            Err(e) => Poll::Ready(Some(Err(e))),
        }
    }
}

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
    shared: Arc<StreamShared>,
    ended: bool,
}

impl FrameStream {
    pub(crate) fn new(shared: Arc<StreamShared>) -> Self {
        Self {
            shared,
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

    /// Whether frame starts are currently inferred from dequeued frames because the
    /// receiver's frame-start events stopped (or never came).
    pub fn frame_sync_fallback(&self) -> bool {
        !self.shared.frame_sync || self.shared.sync_fallback.load(Ordering::Relaxed)
    }

    /// Whether the stream ended because the device went away.
    pub fn is_disconnected(&self) -> bool {
        self.shared.disconnected.load(Ordering::Acquire)
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
        let item = std::task::ready!(self.shared.poll_frame(cx));
        if !matches!(item, Some(Ok(_))) {
            self.ended = true;
        }
        Poll::Ready(item)
    }
}
