//! The running part of a camera: the event thread, buffers, `STREAMON`/`STREAMOFF`, and the
//! clean state every path ends in (sensor in standby and powered down, bridge idle and off,
//! the queue's buffers released).
//!
//! It talks to the bridge and the capture node through [`BridgeDevice`] and [`CaptureDevice`],
//! so the host tests drive it over fakes that inject faults (`fault_tests.rs`).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::bus::StreamState;
use styx_kernel::dma_heap::DmaHeap;

use crate::buffers::{Allocator, BufferMemory, BufferSet, Lender};
use crate::device::{BridgeDevice, CaptureDevice};
use crate::embedded::EmbeddedCapture;
use crate::error::{KernelContext, NativeError, Result, io_errno};
use crate::events::{EventSources, EventThread};
use crate::health::{Fault, Health};
use crate::stream::{FrameStream, SensorSide, StreamShared};

/// How long shutting down waits for the bridge to become idle so it can be switched off
/// (longer than the default acknowledgement timeout).
const POWER_OFF_WAIT: Duration = Duration::from_millis(2500);

/// Where a session's buffers come from.
#[derive(Clone, Debug)]
pub(crate) enum BufferSource {
    Memory(BufferMemory),
    /// memfds imported as `DMABUF` (tests).
    #[cfg(test)]
    Memfd,
}

/// Format of the frames of a stream.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamFormat {
    pub(crate) fourcc: FourCc,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
    pub(crate) size_image: u32,
}

/// Options the session takes from [`crate::CameraOptions`].
#[derive(Clone, Debug)]
pub(crate) struct SessionOptions {
    pub(crate) buffers: u32,
    pub(crate) source: BufferSource,
    pub(crate) max_error_frames: u32,
}

struct Running {
    shared: Arc<StreamShared>,
    events: EventThread,
}

pub(crate) struct Session {
    bridge: Arc<dyn BridgeDevice>,
    sensor: Arc<dyn SensorSide>,
    video: Option<Arc<dyn CaptureDevice>>,
    embedded: Option<Arc<EmbeddedCapture>>,
    frame_sync: bool,
    options: SessionOptions,
    running: Option<Running>,
}

impl Session {
    pub(crate) fn new(
        bridge: Arc<dyn BridgeDevice>,
        sensor: Arc<dyn SensorSide>,
        options: SessionOptions,
    ) -> Self {
        Self {
            bridge,
            sensor,
            video: None,
            embedded: None,
            frame_sync: false,
            options,
            running: None,
        }
    }

    /// Uses `video` as the capture node (`frame_sync`: its frame-start events are
    /// subscribed).
    pub(crate) fn attach_video(
        &mut self,
        video: Arc<dyn CaptureDevice>,
        frame_sync: bool,
    ) -> Result<()> {
        self.video = Some(video);
        self.frame_sync = frame_sync;
        Ok(())
    }

    pub(crate) fn set_embedded(&mut self, embedded: Option<Arc<EmbeddedCapture>>) {
        self.embedded = embedded;
    }

    pub(crate) fn embedded(&self) -> Option<&Arc<EmbeddedCapture>> {
        self.embedded.as_ref()
    }

    pub(crate) fn frame_sync(&self) -> bool {
        self.frame_sync
    }

    pub(crate) fn is_streaming(&self) -> bool {
        self.running.is_some()
    }

    /// Health of the running stream.
    pub(crate) fn health(&self) -> Option<&Arc<Health>> {
        self.running.as_ref().map(|r| &r.shared.health)
    }

    /// Starts streaming: returns once the sensor streams (the bridge's start request was
    /// served). On failure everything started is undone: buffers released, the event thread
    /// stopped, the sensor in standby.
    pub(crate) fn start(&mut self, format: StreamFormat) -> Result<FrameStream> {
        if self.running.is_some() {
            return Err(NativeError::Busy("already streaming".into()));
        }
        let video = Arc::clone(
            self.video
                .as_ref()
                .ok_or(NativeError::State("configure before starting"))?,
        );
        let health = Arc::new(Health::default());
        // Spawned quiesced: it leaves the capture node alone until STREAMON returned.
        let events = EventThread::spawn(EventSources {
            bridge: Arc::clone(&self.bridge),
            video: Arc::clone(&video),
            frame_sync: self.frame_sync,
            sensor: Arc::clone(&self.sensor),
            embedded: self.embedded.clone(),
            health: Arc::clone(&health),
        })
        .step("start the event thread")?;
        let allocator = match &self.options.source {
            BufferSource::Memory(BufferMemory::Mmap) => None,
            BufferSource::Memory(BufferMemory::DmaHeap(name)) => Some(Allocator::Heap(
                DmaHeap::open(name).step(&format!("open dma-heap {name}"))?,
            )),
            #[cfg(test)]
            BufferSource::Memfd => Some(Allocator::Memfd),
        };
        let buffers = BufferSet::allocate_with(
            Arc::clone(&video),
            allocator,
            self.options.buffers,
            format.size_image as usize,
        )
        .map_err(|e| started_error(e, &health))?;
        // Dropping the set frees its buffers again.
        buffers.queue_all().map_err(|e| started_error(e, &health))?;
        let lender = Arc::new(Lender {
            buffers: Arc::new(buffers),
        });
        let mut shared = StreamShared::new(
            Arc::clone(&video),
            events.notify(),
            Arc::clone(&lender),
            Arc::clone(&self.sensor),
            Arc::clone(&health),
            (format.fourcc, format.width, format.height, format.stride),
        );
        shared.embedded = self.embedded.clone();
        shared.frame_sync = self.frame_sync;
        shared.max_error_frames = self.options.max_error_frames;
        let shared = Arc::new(shared);
        let undo = |events: EventThread, embedded: Option<&Arc<EmbeddedCapture>>| {
            if let Some(e) = embedded {
                e.stop();
            }
            let _ = video.stream_off();
            let _ = lender.buffers.release();
            events.join();
            self.sensor.standby();
        };
        if let Some(e) = &self.embedded
            && let Err(err) = e.start()
        {
            undo(events, None);
            return Err(err);
        }
        if let Err(e) = video.stream_on() {
            undo(events, self.embedded.as_ref());
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
            undo(events, self.embedded.as_ref());
            return Err(stream_on_error(
                std::io::Error::from_raw_os_error(errno),
                &health,
            ));
        }
        events.resume();
        self.running = Some(Running {
            shared: Arc::clone(&shared),
            events,
        });
        Ok(FrameStream::new(shared))
    }

    /// Stops streaming. Frame streams end; frames still held keep their memory until dropped
    /// but never go back to the queue, whose buffers are released here.
    pub(crate) fn stop(&mut self) -> Result<()> {
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        let buffers = &running.shared.lender.buffers;
        buffers.retire();
        // The stream ends: wake it if it waits.
        running.shared.notify.wake();
        // Nothing may poll the node while STREAMOFF holds its lock and waits for the bridge.
        running.events.quiesce();
        let result = running.shared.video.stream_off().step("VIDIOC_STREAMOFF");
        // rp1-cfe stops the sensor (the bridge's stop request) when the last node stops: with
        // embedded data that is the embedded node, so the event thread must still serve it.
        if let Some(e) = &self.embedded {
            e.stop();
        }
        running.events.join();
        let released = buffers.release();
        self.sensor.standby();
        match result {
            // The node is gone: there is nothing left to stop.
            Err(e) if e.is_disconnect() => Err(NativeError::Disconnected),
            r => r.and(released),
        }
    }

    /// Stops, puts the sensor in standby and powers it down, and switches the bridge off.
    /// Every step runs; the first error is returned.
    pub(crate) fn shutdown(&mut self) -> Result<()> {
        let stopped = self.stop();
        // Power goes off only once the bridge's stream is idle: a stop request from the
        // receiver that nobody serves any more ends with the bridge's timeout.
        let deadline = Instant::now() + POWER_OFF_WAIT;
        while matches!(self.bridge.is_idle(), Ok(false)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let down = self.sensor.shut_down();
        while matches!(self.bridge.power(), Ok(true)) {
            match self.bridge.set_power(false) {
                Err(e) if io_errno(&e) == Some(libc::EBUSY) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
        stopped.and(down)
    }

    /// Frame starts and acknowledgements seen by the event thread of the running stream.
    pub(crate) fn event_counts(&self) -> Option<(u64, u64)> {
        self.health().map(|h| {
            (
                h.frame_syncs.load(Ordering::Relaxed),
                h.acks.load(Ordering::Relaxed),
            )
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// An error during start, with a disconnect seen by the event thread taking precedence.
fn started_error(e: NativeError, health: &Health) -> NativeError {
    match health.fault() {
        Some(f @ Fault::Disconnected(_)) => f.to_error(),
        _ => e,
    }
}

/// Why `STREAMON` failed, as clearly as the parts know it.
fn stream_on_error(e: std::io::Error, health: &Health) -> NativeError {
    if let Some(f @ Fault::Disconnected(_)) = health.fault() {
        return f.to_error();
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
