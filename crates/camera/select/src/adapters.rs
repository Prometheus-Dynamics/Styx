//! Thin adapters: the three backends' existing stream types behind [`Capture`].
//!
//! No backend crate is changed. Where a backend lacks something the trait asks for, the adapter
//! says so in its docs.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use styx_controls::{FrameMetadata, Update};
use styx_frame::FrameLease;
use styx_libcamera::{Capture as LibcameraStream, CaptureConfig, Error as LibcameraError};
use styx_native_run::{Clock, Session, State};
use styx_v4l2::{State as V4l2State, Stream, StreamConfig};

use crate::capture::{Capture, CaptureError, RateReport, Status};

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
}

impl<'a, C: Clock> NativeCapture<'a, C> {
    /// Starts the session.
    pub fn start(mut session: Session<'a, C>) -> Result<Self, CaptureError> {
        session
            .start()
            .map_err(|e| CaptureError::Failed(e.to_string()))?;
        Ok(Self {
            session,
            achieved_milli: 0,
        })
    }
}

impl<C: Clock> Capture for NativeCapture<'_, C> {
    fn lemnos_skipped(&self) -> u64 {
        self.session.skipped()
    }

    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FrameLease<FrameMetadata>>, CaptureError> {
        let lease = self.session.next_frame(timeout).map_err(|e| match e {
            styx_native_run::Error::Stopped => CaptureError::Stopped,
            other => CaptureError::Failed(other.to_string()),
        })?;
        if let Some(lease) = &lease
            && lease.meta().achieved_fps_milli != 0
        {
            self.achieved_milli = lease.meta().achieved_fps_milli;
        }
        Ok(lease)
    }

    fn set_controls(&mut self, updates: &[Update]) -> Result<(), CaptureError> {
        self.session
            .request_controls(updates)
            .map(|_| ())
            .map_err(|e| CaptureError::Failed(e.to_string()))
    }

    fn status(&self) -> Status {
        let status = self.session.status();
        match &status.state {
            State::Streaming => Status::Streaming,
            State::Stopped => Status::Stopped,
            State::Backoff => Status::Retrying {
                reason: status.last_error.clone().unwrap_or_default(),
            },
            State::Off => Status::Failed {
                reason: status.last_error.clone().unwrap_or_default(),
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
                V4l2State::Retrying { error, .. } => Status::Retrying { reason: error },
                V4l2State::Stopped => Status::Stopped,
                V4l2State::Failed { error } => Status::Failed { reason: error },
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
