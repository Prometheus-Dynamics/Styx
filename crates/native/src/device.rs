//! The two kernel devices a running stream talks to, as traits: the sensor bridge (stream
//! requests, acknowledgements, power) and the receiver's capture node (buffers, streaming,
//! frame-start events). The kernel types implement them; tests use fakes that emulate the
//! kernel's rules and inject faults (`fake.rs`).

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use styx_kernel::bus::{SensorBridge, StreamRequest, StreamState};
use styx_kernel::event::{Event, Events};
use styx_kernel::v4l2::{BufType, DequeuedBuffer, Memory, QueueBuffer, VideoDevice};
use styx_kernel::{Mapping, Wait};

/// The sensor bridge as a running stream uses it.
pub(crate) trait BridgeDevice: AsFd + Send + Sync {
    /// The next pending stream request, without waiting.
    fn try_next_request(&self) -> io::Result<Option<StreamRequest>>;
    /// Acknowledges a request (`ESTALE` when the bridge no longer waits for it).
    fn acknowledge(&self, req: &StreamRequest, result: Result<(), i32>) -> io::Result<()>;
    /// Whether the bridge's supplies and clock are on.
    fn power(&self) -> io::Result<bool>;
    /// Switches the bridge's supplies and clock (refused with `EBUSY` unless idle).
    fn set_power(&self, on: bool) -> io::Result<()>;
    /// Whether the bridge's stream is idle (no start or stop waiting, not streaming).
    fn is_idle(&self) -> io::Result<bool>;
    /// What readiness of the descriptor means "a request is pending".
    fn request_wait(&self) -> Wait {
        Wait::PRIORITY
    }
}

impl BridgeDevice for SensorBridge {
    fn try_next_request(&self) -> io::Result<Option<StreamRequest>> {
        SensorBridge::try_next_request(self)
    }

    fn acknowledge(&self, req: &StreamRequest, result: Result<(), i32>) -> io::Result<()> {
        SensorBridge::acknowledge(self, req, result)
    }

    fn power(&self) -> io::Result<bool> {
        SensorBridge::power(self)
    }

    fn set_power(&self, on: bool) -> io::Result<()> {
        SensorBridge::set_power(self, on)
    }

    fn is_idle(&self) -> io::Result<bool> {
        Ok(SensorBridge::stream_state(self)? == StreamState::Idle)
    }
}

/// A single-planar capture queue as a running stream uses it.
pub(crate) trait CaptureDevice: AsFd + Send + Sync {
    /// The queue type.
    fn buf_type(&self) -> BufType {
        BufType::VideoCapture
    }
    /// `VIDIOC_REQBUFS`: allocates `count` buffers (frees them with 0); returns how many.
    fn request_buffers(&self, memory: Memory, count: u32) -> io::Result<u32>;
    /// Maps buffer `index` (MMAP queues).
    fn map_buffer(&self, index: u32) -> io::Result<Mapping>;
    /// Exports buffer `index` as a dma-buf (MMAP queues).
    fn export_buffer(&self, index: u32) -> io::Result<OwnedFd>;
    /// `VIDIOC_QBUF`.
    fn queue(&self, req: &QueueBuffer<'_>) -> io::Result<()>;
    /// `VIDIOC_DQBUF`, `None` when no buffer is done.
    fn dequeue(&self, memory: Memory) -> io::Result<Option<DequeuedBuffer>>;
    /// `VIDIOC_STREAMON`: blocks until the receiver (and through it the bridge) started.
    fn stream_on(&self) -> io::Result<()>;
    /// `VIDIOC_STREAMOFF`.
    fn stream_off(&self) -> io::Result<()>;
    /// The next pending event (frame starts), without waiting.
    fn dequeue_event(&self) -> io::Result<Option<Event>>;
    /// The descriptor that reports pending events.
    fn event_fd(&self) -> BorrowedFd<'_> {
        self.as_fd()
    }
    /// What readiness of [`Self::event_fd`] means "an event is pending".
    fn event_wait(&self) -> Wait {
        Wait::PRIORITY
    }
}

const CAPTURE: BufType = BufType::VideoCapture;

impl CaptureDevice for VideoDevice {
    fn request_buffers(&self, memory: Memory, count: u32) -> io::Result<u32> {
        Ok(VideoDevice::request_buffers(self, CAPTURE, memory, count)?.count)
    }

    fn map_buffer(&self, index: u32) -> io::Result<Mapping> {
        let mut planes = VideoDevice::map_buffer(self, CAPTURE, index)?;
        if planes.is_empty() {
            return Err(io::Error::other("buffer without planes"));
        }
        Ok(planes.remove(0))
    }

    fn export_buffer(&self, index: u32) -> io::Result<OwnedFd> {
        Ok(VideoDevice::export_buffer(self, CAPTURE, index, 0)?)
    }

    fn queue(&self, req: &QueueBuffer<'_>) -> io::Result<()> {
        Ok(VideoDevice::queue(self, req)?)
    }

    fn dequeue(&self, memory: Memory) -> io::Result<Option<DequeuedBuffer>> {
        Ok(VideoDevice::dequeue(self, CAPTURE, memory)?)
    }

    fn stream_on(&self) -> io::Result<()> {
        Ok(VideoDevice::stream_on(self, CAPTURE)?)
    }

    fn stream_off(&self) -> io::Result<()> {
        Ok(VideoDevice::stream_off(self, CAPTURE)?)
    }

    fn dequeue_event(&self) -> io::Result<Option<Event>> {
        Ok(Events::dequeue_event(self)?)
    }
}
