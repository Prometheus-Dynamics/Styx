//! The running part of a camera: the runtime's [`Camera`] on the Linux receiver (event thread,
//! buffers, `STREAMON`/`STREAMOFF`), the sensor side serving another device's capture
//! (`external.rs`), and the clean state every path ends in (sensor in standby and powered down,
//! bridge idle and off, the queue's buffers released).
//!
//! It talks to the bridge and the capture node through [`BridgeDevice`] and [`CaptureDevice`],
//! so the host tests drive it over fakes that inject faults (`fault_tests.rs`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::bus::StreamState;
use styx_runtime::styx_hal::{BufferSource as HalBufferSource, Bus, ReceiverConfig};
use styx_runtime::{Camera, CameraOptions as RuntimeOptions};

use crate::device::{BridgeDevice, CaptureDevice};
use crate::embedded::EmbeddedCapture;
use crate::error::{KernelContext, NativeError, Result, io_errno};
use crate::events::{EventSources, EventThread, FrameNotify};
use crate::health::Health;
use crate::receiver::{Linux, V4l2Receiver, kernel_start_error};
use crate::stream::{FrameStream, SensorSide};

pub(crate) use crate::receiver::BufferSource;

/// How long shutting down waits for the bridge to become idle so it can be switched off
/// (longer than the default acknowledgement timeout).
const POWER_OFF_WAIT: Duration = Duration::from_millis(2500);

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

/// The sensor side running for another device's capture (see `external.rs`).
struct External {
    events: EventThread,
    health: Arc<Health>,
}

pub(crate) struct Session {
    bridge: Arc<dyn BridgeDevice>,
    sensor: Arc<dyn SensorSide>,
    embedded: Option<Arc<EmbeddedCapture>>,
    frame_sync: bool,
    options: SessionOptions,
    /// The raw route, once a capture node is attached.
    camera: Option<Camera<Linux>>,
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
            embedded: None,
            frame_sync: false,
            options,
            camera: None,
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
        let receiver = V4l2Receiver::new(
            Arc::clone(&self.bridge),
            video,
            frame_sync,
            self.options.source.clone(),
        );
        receiver.set_embedded(self.embedded.clone());
        self.camera = Some(Camera::new(
            Arc::new(receiver),
            Arc::clone(&self.sensor),
            RuntimeOptions {
                frame_sync,
                max_error_frames: self.options.max_error_frames,
            },
        ));
        self.frame_sync = frame_sync;
        Ok(())
    }

    pub(crate) fn set_embedded(&mut self, embedded: Option<Arc<EmbeddedCapture>>) {
        if let Some(c) = &self.camera {
            c.receiver().set_embedded(embedded.clone());
        }
        self.embedded = embedded;
    }

    pub(crate) fn embedded(&self) -> Option<&Arc<EmbeddedCapture>> {
        self.embedded.as_ref()
    }

    pub(crate) fn frame_sync(&self) -> bool {
        self.frame_sync
    }

    pub(crate) fn is_streaming(&self) -> bool {
        self.camera.as_ref().is_some_and(Camera::is_streaming) || self.external.is_some()
    }

    /// Health of the running stream.
    pub(crate) fn health(&self) -> Option<&Arc<Health>> {
        self.camera
            .as_ref()
            .and_then(Camera::health)
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
        let events = EventThread::spawn(
            EventSources {
                bridge: Arc::clone(&self.bridge),
                video,
                frame_sync,
                caller_drives,
                sensor: Arc::clone(&self.sensor),
                embedded: self.embedded.clone(),
                health: Arc::clone(&health),
            },
            Arc::new(FrameNotify::default()),
        )
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
        if self.external.is_some() {
            return Err(NativeError::Busy("already streaming".into()));
        }
        let camera = self
            .camera
            .as_mut()
            .ok_or(NativeError::State("configure before starting"))?;
        camera.receiver().set_size_image(format.size_image as usize);
        let config = ReceiverConfig {
            bus: Bus::Other,
            bus_code: 0,
            fourcc: format.fourcc.0.to_le_bytes(),
            width: format.width,
            height: format.height,
            stride: Some(format.stride),
            buffers: self.options.buffers,
            memory: HalBufferSource::Own,
            embedded: None,
            frame_starts: self.frame_sync,
        };
        let started = Instant::now();
        let (frames, _) = camera.start(&config).map_err(NativeError::from)?;
        Ok(FrameStream::new(
            frames,
            (format.fourcc, format.width, format.height, format.stride),
            started,
        ))
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
        match self.camera.as_mut() {
            Some(c) => c.stop().map_err(NativeError::from),
            None => Ok(()),
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
        let down = self.sensor.shut_down().map_err(NativeError::from);
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
        self.health().map(|h| (h.frame_syncs.get(), h.acks.get()))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
