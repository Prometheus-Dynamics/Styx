//! The Linux receiver: a V4L2 capture node (`rp1-cfe`'s raw node, or any single-planar capture
//! queue) behind the Styx sensor bridge or a kernel-driven sensor, as a
//! [`styx_hal::Receiver`](Receiver) the portable runtime's `Camera` starts, stops and takes
//! frames from.
//!
//! Everything `rp1-cfe` needs stays in here: the event thread with its own `poll(2)` that serves
//! the bridge's start and stop requests and the frame-start events (`events.rs`), its quiesce
//! around `STREAMON`/`STREAMOFF` (the node's lock), the embedded data node started first and
//! stopped after the image node (the receiver stops the sensor when the last node stops), the
//! failed start the bridge reports in its state instead of failing `STREAMON`, and the stop
//! that waits for nothing the bridge needs.
//!
//! ```text
//! configure   REQBUFS (MMAP + EXPBUF, or dma-heap buffers imported), mmap
//! queue       QBUF                                  (any thread: a dropped frame)
//! start       event thread (quiesced), embedded STREAMON, [kernel-driven: frame 0 values],
//!             STREAMON → bridge start request served by the event thread → StartFailed?
//!             → resume (the thread polls the node: frame starts, frames)
//! poll_done   register the waker with the event thread, then DQBUF (+ embedded data)
//! stop        wake the stream, quiesce, STREAMOFF, embedded STREAMOFF, join the thread
//! release     REQBUFS 0 (held frames keep their mappings and dma-bufs)
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use styx_kernel::bus::StreamState;
use styx_kernel::dma_heap::DmaHeap;
use styx_kernel::v4l2::{BufferFlags, Memory};
use styx_runtime::styx_hal::{
    Configured, FrameDone, Instant as HalInstant, Receiver, ReceiverCaps, ReceiverConfig,
    SensorStart, StartOrder, SyncEvent, TimestampPoint,
};
use styx_runtime::{Health, Platform, SensorSide, SyncSource};

use crate::buffers::{Allocator, BufferMemory, BufferSet, V4l2Buffer};
use crate::control::lock;
use crate::device::{BridgeDevice, CaptureDevice};
use crate::embedded::EmbeddedCapture;
use crate::error::{KernelContext, NativeError, Result, io_errno};
use crate::events::{EventSources, EventThread, FrameNotify, VideoSync};
use crate::health::fault_error;

/// The Linux platform: the V4L2 receiver, with the sensor side behind `dyn SensorSide` (the
/// I²C or kernel-driver sensor of a camera, or the mock bus of a host test).
#[derive(Debug)]
pub struct Linux;

impl Platform for Linux {
    type Receiver = V4l2Receiver;
    type Sensor = dyn SensorSide;

    fn attach(receiver: &V4l2Receiver, sensor: &Arc<dyn SensorSide>, health: &Arc<Health>) {
        *lock(&receiver.stream) = Some(StreamParts {
            sensor: Arc::clone(sensor),
            health: Arc::clone(health),
        });
    }
}

/// Where a session's buffers come from.
#[derive(Clone, Debug)]
pub(crate) enum BufferSource {
    Memory(BufferMemory),
    /// memfds imported as `DMABUF` (tests).
    #[cfg(test)]
    Memfd,
}

/// What the event thread of a stream serves.
struct StreamParts {
    sensor: Arc<dyn SensorSide>,
    health: Arc<Health>,
}

/// A V4L2 capture node as the runtime's receiver. See the [module docs](self).
pub struct V4l2Receiver {
    bridge: Arc<dyn BridgeDevice>,
    video: Arc<dyn CaptureDevice>,
    /// Frame-start events are subscribed on the capture node.
    frame_sync: bool,
    source: BufferSource,
    /// Bytes per frame (`sizeimage` of the format set on the node).
    size_image: Mutex<usize>,
    embedded: Mutex<Option<Arc<EmbeddedCapture>>>,
    /// Wakes a waiting frame stream (through the event thread, which polls the node).
    notify: Arc<FrameNotify>,
    /// The configured buffers.
    set: Mutex<Option<Arc<BufferSet>>>,
    /// Their memory type is `DMABUF` (else `MMAP`), for `DQBUF`.
    dmabuf: AtomicBool,
    stream: Mutex<Option<StreamParts>>,
    events: Mutex<Option<EventThread>>,
}

impl std::fmt::Debug for V4l2Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4l2Receiver")
            .field("frame_sync", &self.frame_sync)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl V4l2Receiver {
    pub(crate) fn new(
        bridge: Arc<dyn BridgeDevice>,
        video: Arc<dyn CaptureDevice>,
        frame_sync: bool,
        source: BufferSource,
    ) -> Self {
        Self {
            bridge,
            video,
            frame_sync,
            source,
            size_image: Mutex::new(0),
            embedded: Mutex::new(None),
            notify: Arc::new(FrameNotify::default()),
            set: Mutex::new(None),
            dmabuf: AtomicBool::new(false),
            stream: Mutex::new(None),
            events: Mutex::new(None),
        }
    }

    /// The embedded data node streaming with the image node, if any.
    pub(crate) fn set_embedded(&self, embedded: Option<Arc<EmbeddedCapture>>) {
        *lock(&self.embedded) = embedded;
    }

    /// The bytes per frame of the format set on the node.
    pub(crate) fn set_size_image(&self, size_image: usize) {
        *lock(&self.size_image) = size_image;
    }

    fn embedded(&self) -> Option<Arc<EmbeddedCapture>> {
        lock(&self.embedded).clone()
    }

    fn memory(&self) -> Memory {
        if self.dmabuf.load(Ordering::Relaxed) {
            Memory::DmaBuf
        } else {
            Memory::Mmap
        }
    }

    /// Undoes a start that got as far as the event thread: embedded node stopped, `STREAMOFF`,
    /// the thread joined (the camera then releases the buffers and puts the sensor in
    /// standby).
    fn undo_start(&self, events: EventThread, embedded: Option<&Arc<EmbeddedCapture>>) {
        if let Some(e) = embedded {
            e.stop();
        }
        let _ = self.video.stream_off();
        events.join();
    }
}

impl Receiver for V4l2Receiver {
    type Error = NativeError;
    type Buffer = V4l2Buffer;

    fn caps(&self) -> ReceiverCaps {
        ReceiverCaps {
            frame_start_events: self.frame_sync,
            embedded_data: self.embedded().is_some(),
            timestamp: TimestampPoint::FrameStart,
            start_order: if self.bridge.kernel_driven() {
                StartOrder::ReceiverFirst
            } else {
                StartOrder::ReceiverDriven
            },
            max_buffers: 32,
        }
    }

    /// Allocates the buffers (`cfg.buffers`, at least two) for the format set on the node.
    fn configure(&self, cfg: &ReceiverConfig) -> Result<Configured> {
        let allocator = match &self.source {
            BufferSource::Memory(BufferMemory::Mmap) => None,
            BufferSource::Memory(BufferMemory::DmaHeap(name)) => Some(Allocator::Heap(
                DmaHeap::open(name).step(&format!("open dma-heap {name}"))?,
            )),
            #[cfg(test)]
            BufferSource::Memfd => Some(Allocator::Memfd),
        };
        let size_image = *lock(&self.size_image);
        let set =
            BufferSet::allocate_with(Arc::clone(&self.video), allocator, cfg.buffers, size_image)?;
        self.dmabuf
            .store(set.memory() == Memory::DmaBuf, Ordering::Relaxed);
        let configured = Configured {
            stride: cfg.stride.unwrap_or(0),
            buffer_len: size_image,
            buffers: set.count() as u32,
        };
        *lock(&self.set) = Some(Arc::new(set));
        Ok(configured)
    }

    fn buffer(&self, index: u32) -> Option<V4l2Buffer> {
        let set = lock(&self.set).as_ref().map(Arc::clone)?;
        V4l2Buffer::new(set, index)
    }

    fn queue(&self, index: u32) -> Result<()> {
        let set = lock(&self.set);
        set.as_ref()
            .ok_or(NativeError::State("no buffers"))?
            .queue(index)
    }

    /// Starts streaming: returns once the sensor streams (the bridge's start request was
    /// served by the event thread). The sensor side comes from [`Platform::attach`] (its
    /// start goes through the bridge, not `sensor`).
    fn start<S: SensorStart + Clone>(&self, _sensor: S) -> Result<()> {
        let (sensor, health) = {
            let parts = lock(&self.stream);
            let parts = parts
                .as_ref()
                .ok_or(NativeError::State("the receiver was not attached"))?;
            (Arc::clone(&parts.sensor), Arc::clone(&parts.health))
        };
        let embedded = self.embedded();
        // Spawned quiesced: it leaves the capture node alone until STREAMON returned.
        let events = EventThread::spawn(
            EventSources {
                bridge: Arc::clone(&self.bridge),
                video: Arc::clone(&self.video),
                frame_sync: self.frame_sync,
                caller_drives: false,
                sensor: Arc::clone(&sensor),
                embedded: embedded.clone(),
                health: Arc::clone(&health),
            },
            Arc::clone(&self.notify),
        )
        .step("start the event thread")?;
        if let Some(e) = &embedded
            && let Err(err) = e.start()
        {
            self.undo_start(events, None);
            return Err(err);
        }
        // A kernel driver's sensor starts with the receiver: frame 0's values go first.
        if self.bridge.kernel_driven()
            && let Err(why) = sensor.start_streaming()
        {
            self.undo_start(events, embedded.as_ref());
            return Err(kernel_start_error(why));
        }
        if let Err(e) = self.video.stream_on() {
            self.undo_start(events, embedded.as_ref());
            return Err(stream_on_error(e, &health));
        }
        // By default the bridge does not fail STREAMON for a failed start (rp1-cfe's error
        // path oopses); it reports it in its state, and the receiver must be stopped.
        if let Ok(StreamState::StartFailed) = self.bridge.stream_state() {
            let errno = if health.serve_error().is_some() {
                libc::EIO
            } else {
                libc::ETIMEDOUT
            };
            self.undo_start(events, embedded.as_ref());
            return Err(stream_on_error(
                std::io::Error::from_raw_os_error(errno),
                &health,
            ));
        }
        events.resume();
        *lock(&self.events) = Some(events);
        Ok(())
    }

    /// Stops streaming (the sensor stops on the bridge's request, served by the event thread
    /// until the last node stopped).
    fn stop<S: SensorStart + Clone>(&self, _sensor: S) -> Result<()> {
        // The stream ends: wake it if it waits.
        self.notify.wake();
        let events = lock(&self.events).take();
        // Nothing may poll the node while STREAMOFF holds its lock and waits for the bridge.
        if let Some(ev) = &events {
            ev.quiesce();
        }
        let result = self.video.stream_off().step("VIDIOC_STREAMOFF");
        // rp1-cfe stops the receiver (and asks the bridge to stop the sensor) when the last
        // node stops streaming, which can be the embedded data node: stop it while the event
        // thread still serves the bridge (every stop timed out after 1 s otherwise).
        if let Some(e) = self.embedded() {
            e.stop();
        }
        if let Some(ev) = events {
            ev.join();
        }
        result
    }

    /// Frees the queue's buffers (`REQBUFS 0`); held frames keep theirs.
    fn release(&self) -> Result<()> {
        match lock(&self.set).take() {
            Some(set) => set.release(),
            None => Ok(()),
        }
    }

    /// Frame starts come from the event thread here (it serves them itself); this never
    /// becomes ready while streaming and is woken when the stream stops.
    fn poll_sync(&self, cx: &mut Context<'_>) -> Poll<Result<SyncEvent>> {
        match Receiver::try_sync(self) {
            Ok(Some(e)) => Poll::Ready(Ok(e)),
            Ok(None) => {
                self.notify.register(cx.waker());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// Registers before looking: a frame that arrives in between wakes the caller.
    fn poll_done(&self, cx: &mut Context<'_>) -> Poll<Result<FrameDone>> {
        self.notify.register(cx.waker());
        match self.try_done() {
            Ok(Some(f)) => Poll::Ready(Ok(f)),
            Ok(None) => Poll::Pending,
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// `DQEVENT`: the next frame-start event.
    fn try_sync(&self) -> Result<Option<SyncEvent>> {
        VideoSync(self.video.as_ref())
            .try_sync()
            .map_err(|e| NativeError::kernel("frame-start event", e.0))
    }

    /// `DQBUF`, then the embedded data that arrived with the frame (reported to the sensor
    /// side before the frame's values are looked up).
    fn try_done(&self) -> Result<Option<FrameDone>> {
        let Some(buf) = self.video.dequeue(self.memory()).step("VIDIOC_DQBUF")? else {
            return Ok(None);
        };
        if let Some(e) = lock(&self.embedded).as_ref() {
            e.drain();
        }
        let ns = u64::try_from(buf.timestamp.as_nanos()).unwrap_or(u64::MAX);
        Ok(Some(FrameDone {
            index: buf.index,
            sequence: u64::from(buf.sequence),
            timestamp: HalInstant::from_nanos(ns),
            bytes_used: buf.bytes_used(),
            corrupt: buf.flags.contains(BufferFlags::ERROR),
            stats_slot: None,
        }))
    }
}

/// Setting a kernel-driven sensor's start values failed.
pub(crate) fn kernel_start_error(why: String) -> NativeError {
    NativeError::kernel(
        format!("setting the sensor's start values: {why}"),
        std::io::Error::from_raw_os_error(libc::EIO),
    )
}

/// Why `STREAMON` failed, as clearly as the parts know it.
fn stream_on_error(e: std::io::Error, health: &Health) -> NativeError {
    if let Some(f) = health.fault()
        && f.is_disconnect()
    {
        return fault_error(&f);
    }
    let errno = io_errno(&e);
    let what = match (errno, health.serve_error()) {
        (_, Some(why)) => format!("VIDIOC_STREAMON: the sensor did not start ({why})"),
        (Some(libc::ETIMEDOUT), None) => {
            "VIDIOC_STREAMON: the sensor bridge timed out waiting for the start acknowledgement"
                .to_owned()
        }
        (Some(libc::ENOTCONN), None) => {
            "VIDIOC_STREAMON: nobody serves the sensor bridge's requests".to_owned()
        }
        _ => "VIDIOC_STREAMON".to_owned(),
    };
    NativeError::kernel(what, e)
}
