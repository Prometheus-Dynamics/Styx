//! Lossless stream recording and replay.
//!
//! [`StreamRecorder`] writes frames with their full metadata to a `.styxrec` file: pixel data
//! or bitstream, timestamp and clock, backend sequence numbers, crop, timing and pyramid
//! companions. [`CaptureRequest::replay_source`](crate::capture_api::CaptureRequest::replay_source)
//! plays it back as a camera, so pipelines and the planner run on recorded data exactly as they
//! would live.
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use styx::prelude::*;
//!
//! # let device = CaptureRequest::virtual_source(VirtualSourceConfig::new()).into_device();
//! let handle = CaptureRequest::new(&device).start()?;
//! let mut recorder = StreamRecorder::create("run.styxrec", &device, &handle)?;
//! for _ in 0..100 {
//!     if let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_millis(500)) {
//!         recorder.record(&frame)?;
//!     }
//! }
//! recorder.finish()?;
//! handle.stop();
//!
//! let replay = CaptureRequest::replay_source(ReplaySourceConfig::new("run.styxrec"))?;
//! let handle = replay.open()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub(crate) mod format;

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use styx_capture::prelude::{CaptureDescriptor, Mode, ModeId};
use styx_core::prelude::*;

pub use format::RecordingHeader;

use crate::capture_api::CaptureHandle;
use crate::{BackendHandle, BackendKind, ProbedBackend, ProbedDevice};

/// Errors writing or reading a recording.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("recording io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a styx recording")]
    NotARecording,
    #[error("unsupported recording version {0}")]
    UnsupportedVersion(u16),
    #[error("corrupt recording: {0}")]
    Corrupt(&'static str),
    #[error("frame has no data")]
    EmptyFrame,
    #[error("frame error: {0}")]
    Frame(String),
}

/// How a replay delivers frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ReplayPacing {
    /// At the recorded rate, following the gaps between recorded timestamps. Frames the
    /// consumer is too slow for are dropped by the capture queue as they would be live.
    #[default]
    Realtime,
    /// Every frame, as fast as the consumer takes them (for offline processing and tests).
    Unpaced,
}

/// Writes frames to a `.styxrec` recording.
pub struct StreamRecorder {
    out: BufWriter<File>,
    path: PathBuf,
    frames: u64,
}

impl StreamRecorder {
    /// Create a recording for frames from `handle`, a capture of `device`.
    pub fn create(
        path: impl AsRef<Path>,
        device: &ProbedDevice,
        handle: &CaptureHandle,
    ) -> Result<Self, ReplayError> {
        Self::with_header(
            path,
            &RecordingHeader {
                device: device.identity.clone(),
                backend: handle.backend().to_string(),
                format: handle.mode().format,
                interval: handle.interval(),
            },
        )
    }

    /// Create a recording with an explicit header (e.g. for frames from a pipeline whose
    /// output format differs from the capture mode).
    pub fn with_header(
        path: impl AsRef<Path>,
        header: &RecordingHeader,
    ) -> Result<Self, ReplayError> {
        let path = path.as_ref().to_path_buf();
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&path)?);
        format::write_header(&mut out, header)?;
        Ok(Self {
            out,
            path,
            frames: 0,
        })
    }

    /// Append one frame with its metadata and companions.
    pub fn record(&mut self, frame: &FrameLease) -> Result<(), ReplayError> {
        format::write_frame(&mut self.out, frame)?;
        self.frames += 1;
        Ok(())
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mark the end of the recording and flush it to disk. A recording that is dropped without
    /// `finish` still replays up to the last complete frame.
    pub fn finish(mut self) -> Result<PathBuf, ReplayError> {
        format::write_end(&mut self.out)?;
        self.out.flush()?;
        self.out.get_ref().sync_all()?;
        Ok(self.path.clone())
    }
}

/// A recording to replay as a camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaySourceConfig {
    pub path: PathBuf,
    pub pacing: ReplayPacing,
    /// Start again from the first frame at the end. Timestamps keep increasing across loops.
    pub loop_forever: bool,
}

impl ReplaySourceConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            pacing: ReplayPacing::Realtime,
            loop_forever: false,
        }
    }

    pub fn pacing(mut self, pacing: ReplayPacing) -> Self {
        self.pacing = pacing;
        self
    }

    pub fn loop_forever(mut self, loop_forever: bool) -> Self {
        self.loop_forever = loop_forever;
        self
    }

    /// A device for the recording: the recorded camera's identity with a single replay
    /// backend offering the recorded format.
    pub fn into_device(self) -> Result<ProbedDevice, ReplayError> {
        let header = read_header(&self.path)?;
        let format = header.format;
        let mode = Mode {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: header.interval.into_iter().collect(),
            interval_stepwise: None,
        };
        let mut identity = header.device;
        identity.display = format!("{} (replay)", identity.display);
        Ok(ProbedDevice {
            identity,
            backends: vec![ProbedBackend {
                kind: BackendKind::Replay,
                handle: BackendHandle::Replay {
                    path: self.path,
                    pacing: self.pacing,
                    loop_forever: self.loop_forever,
                },
                descriptor: CaptureDescriptor {
                    modes: vec![mode],
                    controls: Vec::new(),
                },
                properties: vec![("recorded_backend".into(), header.backend)],
            }],
        })
    }
}

/// Read a recording's header.
pub fn read_header(path: impl AsRef<Path>) -> Result<RecordingHeader, ReplayError> {
    format::read_header(&mut BufReader::new(File::open(path)?))
}

/// Open a recording for reading frames directly (without a capture handle).
pub fn open_recording(
    path: impl AsRef<Path>,
) -> Result<(RecordingHeader, RecordingFrames), ReplayError> {
    let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
    let header = format::read_header(&mut reader)?;
    Ok((
        header,
        RecordingFrames {
            reader,
            timestamp_offset: 0,
        },
    ))
}

/// Frames of a recording in order.
pub struct RecordingFrames {
    reader: BufReader<File>,
    timestamp_offset: u64,
}

impl RecordingFrames {
    /// Add `offset` to every timestamp read from here on (used to keep looped replays
    /// increasing).
    pub(crate) fn with_timestamp_offset(mut self, offset: u64) -> Self {
        self.timestamp_offset = offset;
        self
    }
}

impl Iterator for RecordingFrames {
    type Item = Result<FrameLease, ReplayError>;

    fn next(&mut self) -> Option<Self::Item> {
        format::read_frame(&mut self.reader, self.timestamp_offset).transpose()
    }
}

#[cfg(test)]
mod tests;
