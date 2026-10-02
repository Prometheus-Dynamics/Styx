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

/// The sensor side running for another device's capture (see `external.rs`).
struct External {
    events: EventThread,
    health: Arc<Health>,
}

pub(crate) struct Session {
    bridge: Arc<dyn BridgeDevice>,
    sensor: Arc<dyn SensorSide>,
    video: Option<Arc<dyn CaptureDevice>>,
    embedded: Option<Arc<EmbeddedCapture>>,
    frame_sync: bool,
    options: SessionOptions,
    running: Option<Running>,
    external: Option<External>,
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
            external: None,
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
        self.running.is_some() || self.external.is_some()
    }

    /// Health of the running stream.
    pub(crate) fn health(&self) -> Option<&Arc<Health>> {
        self.running
            .as_ref()
            .map(|r| &r.shared.health)
            .or(self.external.as_ref().map(|e| &e.health))
    }

    /// Serves the sensor side while another device owns the capture nodes: the event thread
    /// (spawned quiesced; `video` is the other device's node that sends frame-start events,
    /// opened separately) and the embedded data node. The other device starts streaming next,
    /// then [`Self::resume_external`].
    pub(crate) fn start_external(
        &mut self,
        video: Arc<dyn CaptureDevice>,
        frame_sync: bool,
        caller_drives: bool,
    ) -> Result<Arc<Health>> {
        if self.is_streaming() {
            return Err(NativeError::Busy("already streaming".into()));
        }
        let health = Arc::new(Health::default());
        let events = EventThread::spawn(EventSources {
            bridge: Arc::clone(&self.bridge),
            video,
            frame_sync,
            caller_drives,
            sensor: Arc::clone(&self.sensor),
            embedded: self.embedded.clone(),
            health: Arc::clone(&health),
        })
        .step("start the event thread")?;
        if let Some(e) = &self.embedded
            && let Err(err) = e.start()
        {
            events.join();
            return Err(err);
        }
        self.frame_sync = frame_sync;
        self.external = Some(External {
            events,
            health: Arc::clone(&health),
        });
        Ok(health)
    }

    /// The other device started streaming: fails if the sensor did not start (the bridge does
    /// not fail the receiver's `STREAMON` for that), else lets the event thread use the node.
    pub(crate) fn resume_external(&self) -> Result<()> {
        let Some(ext) = &self.external else {
            return Err(NativeError::State("not started"));
        };
        // A kernel driver's sensor started with the other device's STREAMON, with the values
        // set until then (the caller's start values, written at once while not streaming):
        // the control schedule runs from here.
        if self.bridge.kernel_driven() {
            self.sensor.start_streaming().map_err(kernel_start_error)?;
        }
        if let Ok(StreamState::StartFailed) = self.bridge.stream_state() {
            let why = ext
                .health
                .serve_error()
                .unwrap_or_else(|| "the bridge timed out waiting for the start".into());
            return Err(NativeError::kernel(
                format!("the sensor did not start ({why})"),
                std::io::Error::from_raw_os_error(libc::EIO),
            ));
        }
        ext.events.resume();
        Ok(())
    }

    /// The other device is about to stop (or start) streaming: the event thread leaves the
    /// node alone.
    pub(crate) fn quiesce_external(&self) {
        if let Some(ext) = &self.external {
            ext.events.quiesce();
        }
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
            caller_drives: false,
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
        // A kernel driver's sensor starts with the receiver: frame 0's values go first.
        if self.bridge.kernel_driven()
            && let Err(why) = self.sensor.start_streaming()
        {
            undo(events, self.embedded.as_ref());
            return Err(kernel_start_error(why));
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
        if let Some(ext) = self.external.take() {
            ext.events.quiesce();
            // The embedded node may be the last one streaming: its STREAMOFF stops the
            // receiver, and the bridge's stop request needs the event thread.
            if let Some(e) = &self.embedded {
                e.stop();
            }
            ext.events.join();
            self.sensor.standby();
            return Ok(());
        }
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
        // rp1-cfe stops the receiver (and asks the bridge to stop the sensor) when the last
        // node stops streaming, which can be the embedded data node: stop it while the event
        // thread still serves the bridge (every stop timed out after 1 s otherwise).
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

/// Setting a kernel-driven sensor's start values failed.
fn kernel_start_error(why: String) -> NativeError {
    NativeError::kernel(
        format!("setting the sensor's start values: {why}"),
        std::io::Error::from_raw_os_error(libc::EIO),
    )
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
