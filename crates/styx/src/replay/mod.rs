//! Lossless stream recording and replay.
//!
//! [`StreamRecorder`] writes frames with their full metadata to a recording: pixel data or
//! bitstream, timestamp and clock, backend sequence numbers, crop, timing and pyramid
//! companions. Recordings are MCAP files (feature `replay-mcap`, on by default) that ROS 2
//! tools and Foxglove can open; the experimental `.styxrec` format is available with feature
//! `replay-styxrec`. [`CaptureRequest::replay_source`](crate::capture_api::CaptureRequest::replay_source)
//! plays it back as a camera, so pipelines and the planner run on recorded data exactly as they
//! would live.
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use styx::prelude::*;
//!
//! # let device = CaptureRequest::virtual_source(VirtualSourceConfig::new()).into_device();
//! let handle = CaptureRequest::new(&device).start()?;
//! let mut recorder = StreamRecorder::create("run.mcap", &device, &handle)?;
//! for _ in 0..100 {
//!     if let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_millis(500)) {
//!         recorder.record(&frame)?;
//!     }
//! }
//! recorder.finish()?;
//! handle.stop();
//!
//! let replay = CaptureRequest::replay_source(ReplaySourceConfig::new("run.mcap"))?;
//! let handle = replay.open()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(feature = "replay-mcap")]
mod cdr;
#[cfg(feature = "replay-mcap")]
mod mcap_format;
#[cfg(feature = "replay-styxrec")]
mod styxrec;

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
use std::fs::File;
#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use styx_capture::prelude::{CaptureDescriptor, Mode, ModeId};
use styx_core::prelude::*;

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
use crate::capture_api::CaptureHandle;
use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};

/// What a recording was captured from.
#[derive(Debug, Clone)]
pub struct RecordingHeader {
    pub device: DeviceIdentity,
    /// Backend the frames came from, e.g. `libcamera`.
    pub backend: String,
    pub format: MediaFormat,
    pub interval: Option<Interval>,
}

/// Recording file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamFormat {
    /// MCAP with ROS 2 message types (`.mcap`); opens in Foxglove and ROS 2 tools.
    #[cfg(feature = "replay-mcap")]
    Mcap,
    /// Experimental compact Styx-only format (`.styxrec`).
    #[cfg(feature = "replay-styxrec")]
    Styxrec,
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
impl Default for StreamFormat {
    fn default() -> Self {
        #[cfg(feature = "replay-mcap")]
        return StreamFormat::Mcap;
        #[cfg(not(feature = "replay-mcap"))]
        return StreamFormat::Styxrec;
    }
}

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
    #[error("mcap error: {0}")]
    Mcap(String),
    #[error("no recording format is enabled (features `replay-mcap`, `replay-styxrec`)")]
    NoFormat,
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

/// Writes frames to a recording.
#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
pub struct StreamRecorder {
    out: RecorderOutput,
    path: PathBuf,
    frames: u64,
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
enum RecorderOutput {
    #[cfg(feature = "replay-mcap")]
    Mcap(Box<mcap_format::McapRecorder>),
    #[cfg(feature = "replay-styxrec")]
    Styxrec(std::io::BufWriter<File>),
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
impl StreamRecorder {
    /// Create a recording (in the default format) for frames from `handle`, a capture of
    /// `device`.
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
        Self::with_format(path, header, StreamFormat::default())
    }

    pub fn with_format(
        path: impl AsRef<Path>,
        header: &RecordingHeader,
        format: StreamFormat,
    ) -> Result<Self, ReplayError> {
        let path = path.as_ref().to_path_buf();
        let out = match format {
            #[cfg(feature = "replay-mcap")]
            StreamFormat::Mcap => {
                RecorderOutput::Mcap(Box::new(mcap_format::McapRecorder::create(&path, header)?))
            }
            #[cfg(feature = "replay-styxrec")]
            StreamFormat::Styxrec => {
                let mut out = std::io::BufWriter::with_capacity(1 << 20, File::create(&path)?);
                styxrec::write_header(&mut out, header)?;
                RecorderOutput::Styxrec(out)
            }
        };
        Ok(Self {
            out,
            path,
            frames: 0,
        })
    }

    /// Append one frame with its metadata and companions.
    pub fn record(&mut self, frame: &FrameLease) -> Result<(), ReplayError> {
        match &mut self.out {
            #[cfg(feature = "replay-mcap")]
            RecorderOutput::Mcap(out) => out.record(frame)?,
            #[cfg(feature = "replay-styxrec")]
            RecorderOutput::Styxrec(out) => styxrec::write_frame(out, frame)?,
        }
        self.frames += 1;
        Ok(())
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Complete the recording and flush it to disk. A recording that is dropped without
    /// `finish` (or cut short by a crash) still replays up to its last complete frame.
    pub fn finish(self) -> Result<PathBuf, ReplayError> {
        match self.out {
            #[cfg(feature = "replay-mcap")]
            RecorderOutput::Mcap(out) => out.finish()?,
            #[cfg(feature = "replay-styxrec")]
            RecorderOutput::Styxrec(mut out) => {
                use std::io::Write;
                styxrec::write_end(&mut out)?;
                out.flush()?;
                out.get_ref().sync_all()?;
            }
        }
        Ok(self.path)
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
    open_recording(path).map(|(header, _)| header)
}

/// Open a recording for reading frames directly (without a capture handle). The format is
/// detected from the file.
pub fn open_recording(
    path: impl AsRef<Path>,
) -> Result<(RecordingHeader, RecordingFrames), ReplayError> {
    #[cfg(not(any(feature = "replay-mcap", feature = "replay-styxrec")))]
    {
        let _ = path;
        Err(ReplayError::NoFormat)
    }
    #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
    {
        let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
        let mut magic = [0u8; 8];
        let n = read_up_to(&mut reader, &mut magic)?;
        let start = std::io::Cursor::new(magic[..n].to_vec()).chain(reader);
        #[cfg(feature = "replay-mcap")]
        if magic[..n] == mcap::MAGIC[..] {
            let (header, frames) = mcap_format::McapFrames::open(start)?;
            return Ok((header, RecordingFrames(Frames::Mcap(Box::new(frames)))));
        }
        #[cfg(feature = "replay-styxrec")]
        if magic[..n] == styxrec::MAGIC[..] {
            let mut start = start;
            let header = styxrec::read_header(&mut start)?;
            return Ok((
                header,
                RecordingFrames(Frames::Styxrec {
                    reader: start,
                    offset: 0,
                }),
            ));
        }
        Err(ReplayError::NotARecording)
    }
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
type Source = std::io::Chain<std::io::Cursor<Vec<u8>>, BufReader<File>>;

/// Frames of a recording in order.
pub struct RecordingFrames(Frames);

enum Frames {
    /// Without a recording format nothing can be opened; this only keeps the enum inhabited.
    #[cfg(not(any(feature = "replay-mcap", feature = "replay-styxrec")))]
    #[allow(dead_code)]
    Disabled,
    #[cfg(feature = "replay-mcap")]
    Mcap(Box<mcap_format::McapFrames<Source>>),
    #[cfg(feature = "replay-styxrec")]
    Styxrec { reader: Source, offset: u64 },
}

impl RecordingFrames {
    /// Add `offset` to every timestamp read from here on (used to keep looped replays
    /// increasing).
    #[cfg_attr(
        not(any(feature = "replay-mcap", feature = "replay-styxrec")),
        allow(unused_variables)
    )]
    pub(crate) fn with_timestamp_offset(mut self, offset: u64) -> Self {
        match &mut self.0 {
            #[cfg(not(any(feature = "replay-mcap", feature = "replay-styxrec")))]
            Frames::Disabled => {}
            #[cfg(feature = "replay-mcap")]
            Frames::Mcap(frames) => frames.offset = offset,
            #[cfg(feature = "replay-styxrec")]
            Frames::Styxrec { offset: o, .. } => *o = offset,
        }
        self
    }
}

impl Iterator for RecordingFrames {
    type Item = Result<FrameLease, ReplayError>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            #[cfg(not(any(feature = "replay-mcap", feature = "replay-styxrec")))]
            Frames::Disabled => None,
            #[cfg(feature = "replay-mcap")]
            Frames::Mcap(frames) => frames.next(),
            #[cfg(feature = "replay-styxrec")]
            Frames::Styxrec { reader, offset } => styxrec::read_frame(reader, *offset).transpose(),
        }
    }
}

/// A frame of `format` from recorded bytes: tightly packed visible rows, or a bitstream.
#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
pub(crate) fn frame_from_payload(
    format: MediaFormat,
    timestamp: u64,
    bytes: &[u8],
    bitstream: bool,
) -> Result<FrameLease, ReplayError> {
    if !bitstream {
        return FrameLease::from_visible_bytes(format, timestamp, bytes)
            .map_err(|e| ReplayError::Frame(e.to_string()));
    }
    let len = bytes.len();
    let mut buffer = BufferPool::with_limits(1, len.max(1), 0).lease();
    buffer.resize(len);
    buffer.as_mut_slice().copy_from_slice(bytes);
    Ok(FrameLease::single_plane(
        FrameMeta::new(format, timestamp),
        buffer,
        len,
        len,
    ))
}

#[cfg(test)]
mod tests;
