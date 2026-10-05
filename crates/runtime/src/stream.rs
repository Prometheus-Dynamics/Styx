//! Buffers and frames of a running stream: the pool that lends a receiver's buffers out as
//! [`Frame`]s (dropping one gives its buffer back to the receiver while the stream runs), and
//! the [`FrameStream`] that turns filled buffers into frames, with the per-frame bookkeeping:
//! frame starts inferred from dequeues when the receiver's frame-start events are missing,
//! sequence gaps, corrupt frames in a row, and the fault that ends the stream.
//!
//! Frames may outlive their stream: stopping retires the pool (frames dropped after that keep
//! their buffer out of the receiver) and the pool keeps its buffer handles, so a held frame
//! keeps its memory until it is dropped, while the receiver may already run a newer stream
//! with other buffers.

use alloc::vec::Vec;
use core::task::{Context, Poll};
use core::time::Duration;
use styx_core::sync::{AtomicBool, AtomicUsize, Ordering};

use styx_hal::{Access, ErrorKind, FrameBuffer, FrameDone, HalError, Instant, Receiver};

use crate::camera::{Platform, RunError};
use crate::error::Error;
use crate::health::{Fault, Health};
use crate::sensor::FrameControls;
use crate::side::SensorSide;
use crate::sync::Ref;

/// The buffers of one stream, lent out as frames.
pub struct Pool<R: Receiver + ?Sized> {
    receiver: Ref<R>,
    buffers: Vec<R::Buffer>,
    /// Frames currently lent out.
    outstanding: AtomicUsize,
    /// Whether frames go back to the receiver when dropped. Cleared when the stream stops;
    /// [`Self::retire`] then waits out the frames giving their buffer back right now
    /// (`returning`), so no frame queues a buffer after it.
    live: AtomicBool,
    /// Frames between deciding to give their buffer back and having done it.
    returning: AtomicUsize,
}

impl<R: Receiver + ?Sized> core::fmt::Debug for Pool<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pool")
            .field("buffers", &self.buffers.len())
            .field("outstanding", &self.outstanding())
            .finish_non_exhaustive()
    }
}

impl<R: Receiver + ?Sized> Pool<R> {
    /// The receiver's `count` configured buffers. `None` if it does not have that many.
    pub fn new(receiver: Ref<R>, count: u32) -> Option<Self> {
        let buffers = (0..count)
            .map(|i| receiver.buffer(i))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            receiver,
            buffers,
            outstanding: AtomicUsize::new(0),
            live: AtomicBool::new(true),
            returning: AtomicUsize::new(0),
        })
    }

    /// Gives every buffer to the receiver.
    pub fn queue_all(&self) -> Result<(), R::Error> {
        (0..self.buffers.len() as u32).try_for_each(|i| self.receiver.queue(i))
    }

    /// Buffers in the pool.
    pub fn count(&self) -> usize {
        self.buffers.len()
    }

    /// Frames currently held.
    pub fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::Acquire)
    }

    /// Whether frames still go back to the receiver.
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    /// Stops lending buffers back to the receiver: frames dropped from now on keep their
    /// buffer out of it. Waits for a frame that is giving its buffer back right now.
    pub fn retire(&self) {
        // SeqCst against `give_back`: either it sees `live` cleared, or this sees it counted
        // in `returning` and waits for its queue to finish.
        self.live.store(false, Ordering::SeqCst);
        while self.returning.load(Ordering::SeqCst) != 0 {
            #[cfg(feature = "std")]
            std::thread::yield_now();
            #[cfg(not(feature = "std"))]
            core::hint::spin_loop();
        }
    }

    /// Buffer `index`.
    pub fn buffer(&self, index: u32) -> Option<&R::Buffer> {
        self.buffers.get(index as usize)
    }

    /// Lends buffer `index` out (the receiver filled it).
    pub fn lend(this: &Ref<Self>, index: u32) -> Option<Lease<R>> {
        this.buffers.get(index as usize)?;
        this.outstanding.fetch_add(1, Ordering::AcqRel);
        Some(Lease {
            pool: Ref::clone(this),
            index,
            cpu_access: AtomicBool::new(false),
        })
    }

    /// Returns a lent buffer: queues it again while the stream runs.
    fn give_back(&self, index: u32) {
        self.returning.fetch_add(1, Ordering::SeqCst);
        if self.live.load(Ordering::SeqCst) {
            // Fails only when the device went away; the stream reports that.
            let _ = self.receiver.queue(index);
        }
        self.returning.fetch_sub(1, Ordering::SeqCst);
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A buffer lent out of a [`Pool`]: dropping it gives the buffer back.
pub struct Lease<R: Receiver + ?Sized> {
    pool: Ref<Pool<R>>,
    index: u32,
    cpu_access: AtomicBool,
}

impl<R: Receiver + ?Sized> Lease<R> {
    /// The buffer's index in the receiver.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// The buffer.
    pub fn buffer(&self) -> &R::Buffer {
        &self.pool.buffers[self.index as usize]
    }

    /// The buffer's first `bytes_used` bytes (all of it for 0). The first call starts CPU read
    /// access ([`FrameBuffer::begin_cpu`]: a dma-buf sync on Linux, a cache invalidate on a
    /// Cortex-M7); no end of access follows for reads.
    pub fn bytes(&self, bytes_used: usize) -> &[u8] {
        let buffer = self.buffer();
        if !self.cpu_access.swap(true, Ordering::AcqRel) {
            buffer.begin_cpu(Access::Read);
        }
        let all = buffer.bytes();
        let n = if bytes_used == 0 {
            all.len()
        } else {
            bytes_used.min(all.len())
        };
        &all[..n]
    }
}

impl<R: Receiver + ?Sized> Drop for Lease<R> {
    fn drop(&mut self) {
        // No end of CPU access for reads: on Linux `SYNC_END` only cleans the buffer's lines
        // for the device (22 µs per 1.3 MB frame on the CM5), and a CPU that only read them has
        // none dirty. The next frame in this buffer starts its own access.
        self.pool.give_back(self.index);
    }
}

/// A captured frame. Dropping it gives its buffer back to the receiver.
pub struct Frame<R: Receiver + ?Sized> {
    /// Frame sequence number from the receiver (from 0 with each stream start).
    pub sequence: u64,
    /// Capture timestamp (see [`ReceiverCaps::timestamp`](styx_hal::ReceiverCaps)).
    pub timestamp: Instant,
    /// Payload bytes (0: the whole buffer).
    pub bytes_used: usize,
    /// The receiver flagged the frame as corrupted.
    pub corrupt: bool,
    /// Exposure, gain and frame duration that produced this frame.
    pub controls: Option<FrameControls>,
    pub(crate) lease: Lease<R>,
}

impl<R: Receiver + ?Sized> core::fmt::Debug for Frame<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frame")
            .field("sequence", &self.sequence)
            .field("timestamp", &self.timestamp)
            .field("bytes_used", &self.bytes_used)
            .field("corrupt", &self.corrupt)
            .field("controls", &self.controls)
            .finish_non_exhaustive()
    }
}

impl<R: Receiver + ?Sized> Frame<R> {
    /// The frame's bytes (see [`Lease::bytes`]).
    pub fn data(&self) -> &[u8] {
        self.lease.bytes(self.bytes_used)
    }

    /// The buffer (export handles, device address, length).
    pub fn buffer(&self) -> &R::Buffer {
        self.lease.buffer()
    }

    /// The buffer's index in the receiver.
    pub fn index(&self) -> u32 {
        self.lease.index()
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

/// The frames of a started [`Camera`](crate::Camera).
///
/// Ends (`None`) when the camera stops. A fault ends it after one error item: a disconnect, a
/// receiver error, a sensor that stopped answering, or
/// [`CameraOptions::max_error_frames`](crate::CameraOptions) corrupt frames in a row. Polling
/// registers the caller's waker with the receiver ([`Receiver::poll_done`]) before looking, so
/// it works on any executor, and a frame is only taken from the receiver inside a poll that
/// returns it (dropping a pending poll loses nothing). A superloop polls with a no-op waker.
pub struct FrameStream<P: Platform> {
    receiver: Ref<P::Receiver>,
    pool: Ref<Pool<P::Receiver>>,
    sensor: Ref<P::Sensor>,
    health: Ref<Health>,
    /// Frame starts come from the receiver's events; without them the dequeue drives the
    /// schedule.
    frame_sync: bool,
    /// Consecutive corrupted frames that end the stream (0: never).
    max_error_frames: u32,
    stats: StreamStats,
    last_sequence: u64,
    disconnected: bool,
    /// Frame-start events seen at the last dequeue, and dequeues since the count moved.
    syncs_seen: u64,
    frames_without_sync: u32,
    /// Frame starts are taken from dequeues because the events stopped.
    sync_fallback: bool,
    consecutive_errors: u32,
    ended: bool,
}

impl<P: Platform> core::fmt::Debug for FrameStream<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FrameStream")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// The receiver error of platform `P`.
pub type ReceiverError<P> = <<P as Platform>::Receiver as Receiver>::Error;

/// One item of a [`FrameStream`].
pub type FrameItem<P> = Result<Frame<<P as Platform>::Receiver>, RunError<ReceiverError<P>>>;

impl<P: Platform> FrameStream<P> {
    pub(crate) fn new(
        receiver: Ref<P::Receiver>,
        pool: Ref<Pool<P::Receiver>>,
        sensor: Ref<P::Sensor>,
        health: Ref<Health>,
        frame_sync: bool,
        max_error_frames: u32,
    ) -> Self {
        Self {
            receiver,
            pool,
            sensor,
            health,
            frame_sync,
            max_error_frames,
            stats: StreamStats::default(),
            last_sequence: 0,
            disconnected: false,
            syncs_seen: 0,
            frames_without_sync: 0,
            sync_fallback: false,
            consecutive_errors: 0,
            ended: false,
        }
    }

    /// Frames delivered, dropped and flagged so far.
    pub fn stats(&self) -> StreamStats {
        self.stats
    }

    /// Frames currently held by the application.
    pub fn outstanding(&self) -> usize {
        self.pool.outstanding()
    }

    /// Buffers in the stream.
    pub fn buffer_count(&self) -> usize {
        self.pool.count()
    }

    /// Whether frame starts are currently inferred from received frames because the
    /// receiver's frame-start events stopped (or never came).
    pub fn frame_sync_fallback(&self) -> bool {
        !self.frame_sync || self.sync_fallback
    }

    /// Whether the stream ended because the device went away.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected
    }

    /// Whether the stream ended.
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// The stream's health (counters, the fault that ended it).
    pub fn health(&self) -> &Ref<Health> {
        &self.health
    }

    /// The next frame, `None` once the stream stopped (and after an error item), or
    /// `Pending` with `cx`'s waker registered.
    pub fn poll_frame(&mut self, cx: &mut Context<'_>) -> Poll<Option<FrameItem<P>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        let item = core::task::ready!(self.poll_inner(cx));
        if !matches!(item, Some(Ok(_))) {
            self.ended = true;
        }
        Poll::Ready(item)
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<FrameItem<P>>> {
        if let Some(f) = self.health.fault() {
            if f.is_disconnect() {
                self.disconnected = true;
            }
            return Poll::Ready(Some(Err(RunError::Runtime(Error::Fault(f)))));
        }
        if !self.pool.is_live() {
            return Poll::Ready(None);
        }
        match self.receiver.poll_done(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(done)) => Poll::Ready(Some(self.frame(done))),
            Poll::Ready(Err(_)) if !self.pool.is_live() => Poll::Ready(None),
            Poll::Ready(Err(e)) if e.kind() == ErrorKind::Disconnected => {
                self.disconnected = true;
                Poll::Ready(Some(Err(RunError::Runtime(Error::Disconnected))))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(RunError::Receiver(e)))),
        }
    }

    /// Whether this frame has to drive the control schedule: without frame-start events, or
    /// once they went missing for a few frames (until they come back).
    fn drive_from_dequeue(&mut self) -> bool {
        if !self.frame_sync {
            return true;
        }
        let syncs = self.health.frame_syncs.get();
        if core::mem::replace(&mut self.syncs_seen, syncs) != syncs {
            self.frames_without_sync = 0;
            self.sync_fallback = false;
            return false;
        }
        self.frames_without_sync += 1;
        if self.frames_without_sync >= SYNC_MISSING_FRAMES {
            self.sync_fallback = true;
        }
        self.sync_fallback
    }

    /// The bookkeeping of a filled buffer, and its frame.
    fn frame(&mut self, done: FrameDone) -> FrameItem<P> {
        let seq = done.sequence;
        if self.drive_from_dequeue() {
            // Received after its end: the next frame is starting.
            self.health
                .control_write(self.sensor.frame_start(seq + 1, None));
        }
        let last = core::mem::replace(&mut self.last_sequence, seq + 1);
        if last != 0 && seq + 1 > last + 1 {
            self.stats.dropped += seq - last;
        }
        if done.corrupt {
            self.stats.errors += 1;
            self.consecutive_errors += 1;
            let n = self.consecutive_errors;
            if self.max_error_frames > 0 && n >= self.max_error_frames {
                self.health.fail(Fault::new(
                    ErrorKind::Corrupt,
                    alloc::format!("the receiver flagged {n} frames in a row as corrupted"),
                    None,
                ));
            }
        } else {
            self.consecutive_errors = 0;
        }
        self.stats.frames += 1;
        let controls = self.sensor.applied(seq);
        let lease = Pool::lend(&self.pool, done.index).ok_or(RunError::Runtime(Error::State(
            "the receiver filled a buffer it does not have",
        )))?;
        Ok(Frame {
            sequence: seq,
            timestamp: done.timestamp,
            bytes_used: done.bytes_used,
            corrupt: done.corrupt,
            controls,
            lease,
        })
    }
}

/// How long `CLOCK_MONOTONIC`-style nanoseconds are as a duration.
pub fn instant_duration(i: Instant) -> Duration {
    Duration::from_nanos(i.as_nanos())
}
