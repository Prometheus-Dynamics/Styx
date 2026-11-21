//! Thin adapters: the three backends' existing stream types behind [`Capture`].
//!
//! No backend crate is changed. Where a backend lacks something the trait asks for, the adapter
//! says so in its docs.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use styx_controls::{Control, ControlValue, DeviceKind, FrameMetadata, Update};
use styx_frame::FrameLease;
use styx_libcamera::{Capture as LibcameraStream, CaptureConfig, Error as LibcameraError};
use styx_native_run::{Clock, Event, Ranges, Session, State};
use styx_v4l2::{State as V4l2State, Stream, StreamConfig};

use crate::capture::{Capture, CaptureError, Edge, RateReport, Status};
use crate::controls::{
    Access, ControlsInfo, DeviceInfo, MenuEntry, NATIVE_TEST_PATTERN, Span, StandardInfo,
};

impl<C: Clock> std::fmt::Debug for NativeCapture<'_, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeCapture")
            .field("session", &self.session)
            .finish()
    }
}

/// A native camera: a started [`Session`] (Lemnos sensor, receiver and ISP), with its
/// controls and retries as the session runs them.
///
/// The session must be configured with [`Session::with_controls`] first, since it does not start
/// without it. [`Update::frame`] is honoured: the session applies each value from its frame.
pub struct NativeCapture<'a, C: Clock> {
    session: Session<'a, C>,
    /// The achieved rate of the last frame delivered, from its metadata (0 until one is).
    achieved_milli: u32,
    /// The stall or return the last wait found, for the caller to take ([`Capture::take_edge`]).
    edge: Option<Edge>,
    /// What the controls answer was built from last, and the count that moves when it changes.
    controls_seen: (Option<Ranges>, Option<u16>, u16),
    controls_generation: u64,
}

impl<'a, C: Clock> NativeCapture<'a, C> {
    /// Starts the session. A start refused for a device contract another build's is
    /// [`CaptureError::Contract`], with the contract the session names.
    pub fn start(mut session: Session<'a, C>) -> Result<Self, CaptureError> {
        if let Err(e) = session.start() {
            return Err(match session.status().contract.clone() {
                Some(contract) => CaptureError::Contract {
                    contract,
                    text: e.to_string(),
                },
                None => CaptureError::Failed(e.to_string()),
            });
        }
        let mut capture = Self {
            session,
            achieved_milli: 0,
            edge: None,
            controls_seen: (None, None, 0),
            controls_generation: 1,
        };
        capture.track_controls();
        Ok(capture)
    }

    /// Moves the controls count when the mode's ranges, the frame length in force or the test
    /// pattern changed. Plain values compared: nothing allocated.
    fn track_controls(&mut self) {
        let seen = (
            self.session.ranges(),
            self.session.test_patterns(),
            self.session.test_pattern(),
        );
        if seen != self.controls_seen {
            self.controls_seen = seen;
            self.controls_generation += 1;
        }
    }
}

/// A native camera's controls: exposure, gain and frame rate with the ranges the selected mode gives
/// (the sensor contract's `Mode`), and the sensor's test patterns as a numbered menu (the contract
/// names none). Nothing else is settable on this path, so nothing else is listed.
fn native_controls(ranges: Option<Ranges>, patterns: Option<u16>, pattern: u16) -> ControlsInfo {
    let mut info = ControlsInfo::default();
    if let Some(r) = ranges {
        let manual = |(min, max): (u32, u32)| Span::Manual { min, max };
        info.standard.push(StandardInfo {
            control: Control::Exposure,
            range: Some(manual(r.exposure_us)),
            // The sensor moves in exposure codes (a fraction of its line time), not whole
            // microseconds: no even step in this unit.
            step: None,
            default: Some(ControlValue::Manual(r.initial.exposure_us)),
            effective: Some((r.exposure_us.0, r.exposure_effective_max_us)),
            auto: Some(false),
            access: Access::ReadWrite,
        });
        info.standard.push(StandardInfo {
            control: Control::Gain,
            range: r.gain_milli.map(manual),
            step: r.gain_step_milli,
            default: Some(ControlValue::Manual(r.initial.gain_milli)),
            effective: r.gain_milli,
            auto: Some(false),
            access: Access::ReadWrite,
        });
        info.standard.push(StandardInfo {
            control: Control::FrameRate,
            range: r.frame_rate_milli.map(manual),
            step: None,
            default: Some(ControlValue::Manual(r.initial.frame_rate_milli)),
            effective: r.frame_rate_milli,
            auto: Some(false),
            access: Access::ReadWrite,
        });
    }
    if let Some(count) = patterns.filter(|n| *n > 0) {
        info.device.push(DeviceInfo {
            id: NATIVE_TEST_PATTERN,
            name: "test_pattern".to_string(),
            kind: DeviceKind::Menu,
            min: Some(0),
            max: Some(i64::from(count)),
            step: Some(1),
            default: Some(0),
            effective: Some((0, i64::from(count))),
            value: Some(i64::from(pattern)),
            standard: None,
            access: Access::ReadWrite,
            menu: (0..=i64::from(count))
                .map(|value| MenuEntry { value, label: None })
                .collect(),
        });
    }
    info
}

impl<C: Clock> Capture for NativeCapture<'_, C> {
    fn lemnos_skipped(&self) -> u64 {
        self.session.skipped()
    }

    fn hold(&self) -> Option<u32> {
        self.session.hold()
    }

    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FrameLease<FrameMetadata>>, CaptureError> {
        let event = self.session.next_event(timeout).map_err(|e| match e {
            styx_native_run::Error::Stopped => CaptureError::Stopped,
            other => CaptureError::Failed(other.to_string()),
        })?;
        self.track_controls();
        match event {
            Event::Frame(lease) => {
                if lease.meta().achieved_fps_milli != 0 {
                    self.achieved_milli = lease.meta().achieved_fps_milli;
                }
                Ok(Some(lease))
            }
            Event::Idle => Ok(None),
            Event::Stalled { since_ns, text } => {
                self.edge = Some(Edge::Stalled { since_ns, text });
                Ok(None)
            }
            Event::Resumed(carried) => {
                self.edge = Some(Edge::Resumed { carried });
                Ok(None)
            }
        }
    }

    fn take_edge(&mut self) -> Option<Edge> {
        self.edge.take()
    }

    fn set_controls(&mut self, updates: &[Update]) -> Result<(), CaptureError> {
        self.session
            .request_controls(updates)
            .map(|_| ())
            .map_err(|e| CaptureError::Failed(e.to_string()))
    }

    fn controls_info(&self) -> ControlsInfo {
        native_controls(
            self.session.ranges(),
            self.session.test_patterns(),
            self.session.test_pattern(),
        )
    }

    fn controls_generation(&self) -> u64 {
        self.controls_generation
    }

    fn set_device(&mut self, id: u32, value: i64) -> Result<(), CaptureError> {
        if id != NATIVE_TEST_PATTERN {
            return Err(CaptureError::Failed(format!("no device control {id:#x}")));
        }
        let code = u16::try_from(value)
            .map_err(|_| CaptureError::Failed(format!("no test pattern {value}")))?;
        self.session
            .set_test_pattern(code)
            .map_err(|e| CaptureError::Failed(e.to_string()))?;
        self.track_controls();
        Ok(())
    }

    fn status(&self) -> Status {
        let status = self.session.status();
        match &status.state {
            // A stall: the camera is retried and the stream stays attached (health, not a fault).
            State::Streaming if status.stall.is_some() => Status::Retrying {
                reason: status.stall.clone().unwrap_or_default(),
                layout: status.layout,
                contract: status.contract.clone(),
            },
            State::Streaming => Status::Streaming,
            State::Stopped => Status::Stopped,
            State::Backoff => Status::Retrying {
                reason: status.last_error.clone().unwrap_or_default(),
                layout: status.layout,
                contract: status.contract.clone(),
            },
            State::Off => Status::Failed {
                reason: status.last_error.clone().unwrap_or_default(),
                layout: status.layout,
                contract: status.contract.clone(),
            },
        }
    }

    fn rate(&self) -> RateReport {
        // The rate in force on the last frame; before a frame, the rate the camera started at.
        let start = self.session.frame_rate();
        let achieved = match self.achieved_milli {
            0 => start.map(|r| r.achieved_milli),
            milli => Some(milli),
        };
        RateReport {
            achieved_milli: achieved.filter(|milli| *milli != 0),
            clamped: start.is_some_and(|r| r.clamped),
        }
    }

    fn stop(&mut self) {
        let _ = self.session.stop();
    }
}

impl std::fmt::Debug for V4l2Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4l2Capture")
            .field("running", &self.stream.is_some())
            .finish()
    }
}

/// A V4L2 capture node: a running [`Stream`] from the `lemnos-v4l2` library.
///
/// [`Update::frame`] is not honoured: `Stream::set_control` applies a value on the next frame the
/// driver takes, and that is all the stream offers.
pub struct V4l2Capture {
    stream: Option<Stream>,
}

impl V4l2Capture {
    /// Opens and starts the node at `path` (the listing's key) with `config`.
    pub fn start(path: &str, config: StreamConfig) -> Result<Self, CaptureError> {
        let stream =
            Stream::start(path, config).map_err(|e| CaptureError::Failed(e.to_string()))?;
        Ok(Self {
            stream: Some(stream),
        })
    }
}

impl Capture for V4l2Capture {
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FrameLease<FrameMetadata>>, CaptureError> {
        let stream = self.stream.as_ref().ok_or(CaptureError::Stopped)?;
        match stream.frames().recv_timeout(timeout) {
            Ok(lease) => Ok(Some(lease)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Stopped),
        }
    }

    fn set_controls(&mut self, updates: &[Update]) -> Result<(), CaptureError> {
        let stream = self.stream.as_ref().ok_or(CaptureError::Stopped)?;
        for update in updates {
            stream
                .set_control(update.control, update.value)
                .map_err(|e| CaptureError::Failed(e.to_string()))?;
        }
        Ok(())
    }

    fn status(&self) -> Status {
        match &self.stream {
            None => Status::Stopped,
            Some(stream) => match stream.status().state {
                V4l2State::Streaming => Status::Streaming,
                V4l2State::Retrying { error, .. } => Status::Retrying {
                    reason: error,
                    layout: None,
                    contract: None,
                },
                V4l2State::Stopped => Status::Stopped,
                V4l2State::Failed { error } => Status::Failed {
                    reason: error,
                    layout: None,
                    contract: None,
                },
            },
        }
    }

    fn stop(&mut self) {
        // Dropping the stream stops it and waits for its thread.
        self.stream = None;
    }
}

impl std::fmt::Debug for LibcameraCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibcameraCapture")
            .field("running", &self.camera.is_some())
            .finish()
    }
}

/// A libcamera camera. Frames are pushed by libcamera; they are queued here for
/// [`Capture::next_frame`] up to the configured buffer count. When the queue is full the frame is
/// dropped (its buffer goes back to libcamera), never waited for.
///
/// [`Update::frame`] is not honoured: libcamera's `set_control` applies from the next request
/// queued.
pub struct LibcameraCapture {
    camera: Option<LibcameraStream>,
    frames: Receiver<FrameLease<FrameMetadata>>,
}

impl LibcameraCapture {
    /// Opens camera `id` (the listing's key) with `config`.
    pub fn open(id: &str, config: CaptureConfig) -> Result<Self, CaptureError> {
        let (tx, frames): (SyncSender<FrameLease<FrameMetadata>>, _) =
            mpsc::sync_channel(config.buffers.max(1) as usize);
        let sink = Box::new(move |lease: FrameLease<FrameMetadata>| {
            // A full queue (or a gone reader) drops the lease here: its buffer goes back to
            // libcamera. Nothing waits.
            let _ = tx.try_send(lease);
        });
        let camera = LibcameraStream::open(id, config, sink).map_err(libcamera_error)?;
        Ok(Self {
            camera: Some(camera),
            frames,
        })
    }
}

fn libcamera_error(e: LibcameraError) -> CaptureError {
    match e {
        LibcameraError::Unsupported(what) => CaptureError::Unsupported(what),
        other => CaptureError::Failed(other.to_string()),
    }
}

impl Capture for LibcameraCapture {
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FrameLease<FrameMetadata>>, CaptureError> {
        if self.camera.is_none() {
            return Err(CaptureError::Stopped);
        }
        match self.frames.recv_timeout(timeout) {
            Ok(lease) => Ok(Some(lease)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Stopped),
        }
    }

    fn set_controls(&mut self, updates: &[Update]) -> Result<(), CaptureError> {
        let camera = self.camera.as_ref().ok_or(CaptureError::Stopped)?;
        for update in updates {
            camera
                .set_control(*update)
                .map_err(|e| CaptureError::Failed(e.to_string()))?;
        }
        Ok(())
    }

    fn status(&self) -> Status {
        match self.camera {
            Some(_) => Status::Streaming,
            None => Status::Stopped,
        }
    }

    fn stop(&mut self) {
        self.camera = None;
    }
}
