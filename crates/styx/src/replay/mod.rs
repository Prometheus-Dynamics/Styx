//! Lossless stream recording and replay.
//!
//! [`StreamRecorder`] writes frames with their full metadata to a recording: pixel data or
//! bitstream, timestamp and clock, backend sequence numbers, crop, timing and pyramid
//! companions. Recordings are MCAP files (feature `replay-mcap`, opt-in) that ROS 2
//! tools and Foxglove can open; the experimental `.styxrec` format is available with feature
//! `replay-styxrec`. [`CaptureRequest::replay_source`](crate::capture_api::CaptureRequest::replay_source)
//! plays it back as a camera, so pipelines and the planner run on recorded data exactly as they
//! would live.
//!
#![cfg_attr(
    any(feature = "replay-mcap", feature = "replay-styxrec"),
    doc = "```rust,no_run"
)]
// Without a recording format there is no `StreamRecorder` to show.
#![cfg_attr(
    not(any(feature = "replay-mcap", feature = "replay-styxrec")),
    doc = "```rust,ignore"
)]
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
#[cfg(feature = "replay-mcap")]
mod mcap_frames;
#[cfg(feature = "replay-mcap")]
mod mcap_host;
#[cfg(feature = "replay-mcap")]
mod mcap_native;
#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
pub(crate) mod media;
#[cfg(feature = "replay-mcap")]
mod raw_storage;
#[cfg(feature = "replay-mcap")]
mod recording;
#[cfg(feature = "replay-mcap")]
pub use raw_storage::RawStorage;
#[cfg(feature = "replay-mcap")]
pub use recording::{ChannelId, ChannelSchema, RecordCompression, RecorderOptions, RecorderStats};
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
#[non_exhaustive]
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
    /// MCAP sequence of each host channel's next message, by id from the first.
    #[cfg(feature = "replay-mcap")]
    host_sequences: Vec<u32>,
    /// Write counters when there is no writer thread (it keeps its own).
    #[cfg(feature = "replay-mcap")]
    counters: recording::Counters,
    /// How 16-bit Bayer frames are stored.
    #[cfg(feature = "replay-mcap")]
    raw_storage: RawStorage,
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
enum RecorderOutput {
    #[cfg(feature = "replay-mcap")]
    Mcap(Box<mcap_format::McapRecorder>),
    /// Written from a background thread ([`StreamRecorder::mcap`]).
    #[cfg(feature = "replay-mcap")]
    McapQueued(recording::Background),
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
        Ok(Self::with_output(out, path))
    }

    fn with_output(out: RecorderOutput, path: PathBuf) -> Self {
        Self {
            out,
            path,
            frames: 0,
            #[cfg(feature = "replay-mcap")]
            host_sequences: Vec::new(),
            #[cfg(feature = "replay-mcap")]
            counters: recording::Counters::default(),
            #[cfg(feature = "replay-mcap")]
            raw_storage: RawStorage::AsIs,
        }
    }

    /// An MCAP recording written as `options` say: from a background thread with a bounded
    /// queue (frames dropped and counted when the disk falls behind), with chunk compression,
    /// clock samples, and the host's own channels ([`Self::add_channel`]), metadata and
    /// attachments.
    #[cfg(feature = "replay-mcap")]
    pub fn mcap(
        path: impl AsRef<Path>,
        header: &RecordingHeader,
        options: RecorderOptions,
    ) -> Result<Self, ReplayError> {
        let path = path.as_ref().to_path_buf();
        let out = mcap_format::McapRecorder::create_with(&path, header, &options)?;
        let out = match options.queue_bytes {
            0 => RecorderOutput::Mcap(Box::new(out)),
            capacity => RecorderOutput::McapQueued(recording::Background::start(out, capacity)?),
        };
        let mut rec = Self::with_output(out, path);
        rec.raw_storage = options.raw_storage;
        Ok(rec)
    }

    /// Append one frame with its metadata and companions. With a writer thread the frame is
    /// copied and queued (or dropped when the queue is full, see [`Self::stats`]).
    pub fn record(&mut self, frame: &FrameLease) -> Result<(), ReplayError> {
        #[cfg(feature = "replay-mcap")]
        let sequence = self.frames as u32;
        match &mut self.out {
            #[cfg(feature = "replay-mcap")]
            RecorderOutput::Mcap(out) => {
                let converted = raw_storage::convert(frame, self.raw_storage)?;
                let msgs =
                    mcap_format::McapRecorder::encode_frame(converted.as_ref().unwrap_or(frame))?;
                recording::apply(
                    out,
                    recording::Item::Frame { sequence, msgs },
                    &self.counters,
                )?
            }
            #[cfg(feature = "replay-mcap")]
            RecorderOutput::McapQueued(out) => {
                let converted = raw_storage::convert(frame, self.raw_storage)?;
                let msgs =
                    mcap_format::McapRecorder::encode_frame(converted.as_ref().unwrap_or(frame))?;
                out.push(recording::Item::Frame { sequence, msgs })?
            }
            #[cfg(feature = "replay-styxrec")]
            RecorderOutput::Styxrec(out) => styxrec::write_frame(out, frame)?,
        }
        self.frames += 1;
        Ok(())
    }

    /// Frames and host messages written and dropped so far (MCAP recordings).
    #[cfg(feature = "replay-mcap")]
    pub fn stats(&self) -> RecorderStats {
        match &self.out {
            RecorderOutput::McapQueued(out) => out.stats(),
            _ => self.counters.snapshot(0),
        }
    }

    /// Adds a channel of the host's own on `topic` (an MCAP recording only): its messages'
    /// encoding (`json`, `cdr`, `protobuf`, ...), their schema if they have one, and the
    /// channel's metadata. Replay ignores channels it does not know.
    #[cfg(feature = "replay-mcap")]
    pub fn add_channel(
        &mut self,
        topic: &str,
        schema: Option<ChannelSchema>,
        message_encoding: &str,
        metadata: std::collections::BTreeMap<String, String>,
    ) -> Result<ChannelId, ReplayError> {
        let id = u16::try_from(self.host_sequences.len())
            .ok()
            .and_then(|n| recording::FIRST_HOST_CHANNEL.checked_add(n))
            .ok_or_else(|| ReplayError::Mcap("too many host channels".into()))?;
        self.send(recording::Item::Channel {
            id,
            topic: topic.to_string(),
            schema,
            message_encoding: message_encoding.to_string(),
            metadata,
        })?;
        self.host_sequences.push(0);
        Ok(ChannelId(id))
    }

    /// A message on a host channel at `log_time_ns` (on the clock the host keeps; the frames'
    /// clock and `/styx/clock` relate it to the others). Dropped and counted like frames when
    /// the queue is full.
    #[cfg(feature = "replay-mcap")]
    pub fn write(
        &mut self,
        channel: ChannelId,
        log_time_ns: u64,
        data: &[u8],
    ) -> Result<(), ReplayError> {
        let slot = usize::from(channel.0.wrapping_sub(recording::FIRST_HOST_CHANNEL));
        let Some(sequence) = self.host_sequences.get_mut(slot) else {
            return Err(ReplayError::Mcap("unknown host channel".into()));
        };
        let item = recording::Item::Message {
            channel: channel.0,
            sequence: *sequence,
            log_time: log_time_ns,
            publish_time: log_time_ns,
            data: data.to_vec(),
        };
        *sequence = sequence.wrapping_add(1);
        self.send(item)
    }

    /// A metadata record (e.g. calibration, mount, field layout as key/value pairs).
    #[cfg(feature = "replay-mcap")]
    pub fn add_metadata(
        &mut self,
        name: &str,
        metadata: std::collections::BTreeMap<String, String>,
    ) -> Result<(), ReplayError> {
        self.send(recording::Item::Metadata {
            name: name.to_string(),
            metadata,
        })
    }

    /// An attachment: a file stored whole in the recording (`media_type` e.g.
    /// `application/json`).
    #[cfg(feature = "replay-mcap")]
    pub fn add_attachment(
        &mut self,
        name: &str,
        media_type: &str,
        log_time_ns: u64,
        data: Vec<u8>,
    ) -> Result<(), ReplayError> {
        self.send(recording::Item::Attachment {
            name: name.to_string(),
            media_type: media_type.to_string(),
            log_time: log_time_ns,
            data,
        })
    }

    #[cfg(feature = "replay-mcap")]
    fn send(&mut self, item: recording::Item) -> Result<(), ReplayError> {
        match &mut self.out {
            RecorderOutput::Mcap(out) => recording::apply(out, item, &self.counters),
            RecorderOutput::McapQueued(out) => out.push(item),
            #[cfg(feature = "replay-styxrec")]
            RecorderOutput::Styxrec(_) => Err(ReplayError::Mcap(
                "host channels, metadata and attachments need an MCAP recording".into(),
            )),
        }
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
            #[cfg(feature = "replay-mcap")]
            RecorderOutput::McapQueued(out) => out.finish()?,
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

/// The timestamps replayed frames carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReplayTimestamps {
    /// As recorded (on the recording's clock), increasing across loops.
    #[default]
    Recorded,
    /// When each frame is delivered (`CLOCK_MONOTONIC`): consistent with live frames and with
    /// pausing, seeking and the speed.
    Rebased,
}

/// A recording to replay as a camera. Its capture has playback controls
/// (`capture_api::replay_controls`): play/pause, speed, loop, seek, step, position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaySourceConfig {
    pub path: PathBuf,
    /// The speed it starts at: the recorded rate, or every frame as fast as consumers take
    /// them (the `replay_speed` control changes it).
    pub pacing: ReplayPacing,
    /// Start again from the first frame at the end. Timestamps keep increasing across loops.
    pub loop_forever: bool,
    pub timestamps: ReplayTimestamps,
    /// At the end (not looping), keep the capture open, paused on the last frame, for a seek
    /// or a loop (a player); `false` (default): the capture ends, consumers see `Closed`.
    pub hold_at_end: bool,
    /// Process raw Bayer frames with the software ISP (feature `replay-softisp`, in `native`): the replay then
    /// delivers NV12. `None` (default): frames as recorded.
    pub isp: Option<ReplayIsp>,
    /// The rate (fps) of video files without one (`.mjpeg` streams, JPEG directories; an AVI
    /// has its own). `None` (default): 30.
    pub frame_rate: Option<u32>,
}

/// Raw frames of a recording processed as the camera would (`ReplaySourceConfig::soft_isp`):
/// the software ISP with the sensor's description and tuning, and each frame's recorded
/// exposure, gains and white balance (no AE or AWB). Frames keep their recorded metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayIsp {
    /// A built-in sensor description's name (e.g. `ov9782`), for its modes and tuning.
    pub sensor: String,
    /// Threads of the software ISP.
    pub threads: usize,
}

/// The replay options carried in a replay backend's properties (`timestamps`, `hold_at_end`,
/// `isp`).
pub(crate) fn replay_options(
    properties: &[(String, String)],
) -> (ReplayTimestamps, bool, Option<ReplayIsp>, Option<f32>) {
    let get = |k: &str| {
        properties
            .iter()
            .find(|(p, _)| p == k)
            .map(|(_, v)| v.as_str())
    };
    let timestamps = match get(TIMESTAMPS_PROPERTY) {
        Some("rebased") => ReplayTimestamps::Rebased,
        _ => ReplayTimestamps::Recorded,
    };
    let fps = get(FPS_PROPERTY).and_then(|f| f.parse().ok());
    let isp = get(ISP_SENSOR_PROPERTY).map(|sensor| ReplayIsp {
        sensor: sensor.to_string(),
        threads: get(ISP_THREADS_PROPERTY)
            .and_then(|t| t.parse().ok())
            .unwrap_or(2),
    });
    (timestamps, get(END_PROPERTY) == Some("hold"), isp, fps)
}

const FPS_PROPERTY: &str = "replay_fps";

const ISP_SENSOR_PROPERTY: &str = "replay_isp_sensor";
const ISP_THREADS_PROPERTY: &str = "replay_isp_threads";

const TIMESTAMPS_PROPERTY: &str = "replay_timestamps";
const END_PROPERTY: &str = "replay_end";

impl ReplaySourceConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            pacing: ReplayPacing::Realtime,
            loop_forever: false,
            timestamps: ReplayTimestamps::Recorded,
            hold_at_end: false,
            isp: None,
            frame_rate: None,
        }
    }

    /// The rate of video files without one (see [`Self::frame_rate`]).
    pub fn frame_rate(mut self, fps: u32) -> Self {
        self.frame_rate = Some(fps);
        self
    }

    /// Process raw frames as the camera's ISP would (see [`ReplayIsp`]), with sensor `sensor`'s
    /// description and tuning, on two threads. Recordings of other formats replay as they are.
    pub fn soft_isp(mut self, sensor: &str) -> Self {
        self.isp = Some(ReplayIsp {
            sensor: sensor.to_string(),
            threads: 2,
        });
        self
    }

    pub fn timestamps(mut self, timestamps: ReplayTimestamps) -> Self {
        self.timestamps = timestamps;
        self
    }

    pub fn hold_at_end(mut self, hold: bool) -> Self {
        self.hold_at_end = hold;
        self
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
        let header = open_source(&self.path, self.frame_rate.map(|f| f as f32))?.0;
        let mut format = header.format;
        // Processed raw frames come out in NV12.
        if self.isp.is_some() && format.code.is_bayer_raw() {
            format = MediaFormat::new(FourCc::NV12, format.resolution, ColorSpace::Srgb);
        }
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
                    controls: crate::capture_api::replay_controls::metas(),
                },
                properties: vec![
                    ("recorded_backend".into(), header.backend),
                    (
                        TIMESTAMPS_PROPERTY.into(),
                        match self.timestamps {
                            ReplayTimestamps::Recorded => "recorded",
                            ReplayTimestamps::Rebased => "rebased",
                        }
                        .into(),
                    ),
                    (
                        END_PROPERTY.into(),
                        if self.hold_at_end { "hold" } else { "close" }.into(),
                    ),
                ]
                .into_iter()
                .chain(
                    self.frame_rate
                        .map(|f| (FPS_PROPERTY.to_string(), f.to_string())),
                )
                .chain(self.isp.iter().flat_map(|isp| {
                    [
                        (ISP_SENSOR_PROPERTY.into(), isp.sensor.clone()),
                        (ISP_THREADS_PROPERTY.into(), isp.threads.to_string()),
                    ]
                }))
                .collect(),
            }],
        })
    }
}

/// Read a recording's header.
pub fn read_header(path: impl AsRef<Path>) -> Result<RecordingHeader, ReplayError> {
    open_recording(path).map(|(header, _)| header)
}

/// A recording, or a video file a replay plays (`media`: Motion JPEG, AVI, JPEG directories;
/// `fps` for files without a rate).
pub(crate) fn open_source(
    path: &Path,
    fps: Option<f32>,
) -> Result<(RecordingHeader, RecordingFrames), ReplayError> {
    #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
    return media::open_source(path, fps);
    #[cfg(not(any(feature = "replay-mcap", feature = "replay-styxrec")))]
    {
        let _ = fps;
        open_recording(path)
    }
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
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        open_recording_reader(file, len)
    }
}

/// Open a recording from any reader, e.g. one held in memory or streamed over the network.
/// `len` is the recording's size in bytes, or an upper bound: records claiming to be larger
/// are rejected as corrupt instead of being buffered.
pub fn open_recording_reader(
    reader: impl std::io::Read + Send + 'static,
    len: u64,
) -> Result<(RecordingHeader, RecordingFrames), ReplayError> {
    #[cfg(not(any(feature = "replay-mcap", feature = "replay-styxrec")))]
    {
        let _ = (reader, len);
        Err(ReplayError::NoFormat)
    }
    #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
    {
        let mut reader: BufReader<Box<dyn Read + Send>> =
            // Records are read one at a time and large reads bypass the buffer.
            BufReader::with_capacity(64 << 10, Box::new(reader));
        let mut magic = [0u8; 8];
        let n = read_up_to(&mut reader, &mut magic)?;
        let start = std::io::Cursor::new(magic[..n].to_vec()).chain(reader);
        // `.styxrec` frames are bounded by their own headers.
        #[cfg(not(feature = "replay-mcap"))]
        let _ = len;
        #[cfg(feature = "replay-mcap")]
        if magic[..n] == mcap::MAGIC[..] {
            let limit = usize::try_from(len).unwrap_or(usize::MAX);
            let (header, frames) = mcap_frames::McapFrames::open(start, limit)?;
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
type Source = std::io::Chain<std::io::Cursor<Vec<u8>>, BufReader<Box<dyn Read + Send>>>;

/// Frames of a recording in order.
pub struct RecordingFrames(Frames);

enum Frames {
    /// Without a recording format nothing can be opened; this only keeps the enum inhabited.
    #[cfg(not(any(feature = "replay-mcap", feature = "replay-styxrec")))]
    #[allow(dead_code)]
    Disabled,
    #[cfg(feature = "replay-mcap")]
    Mcap(Box<mcap_frames::McapFrames<Source>>),
    #[cfg(feature = "replay-styxrec")]
    Styxrec { reader: Source, offset: u64 },
    #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
    Media(Box<media::MediaFrames>),
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
            #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
            Frames::Media(frames) => frames.offset = offset,
        }
        self
    }

    #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
    pub(crate) fn media(frames: media::MediaFrames) -> Self {
        Self(Frames::Media(Box::new(frames)))
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
            #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
            Frames::Media(frames) => frames.next(),
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

#[cfg(all(test, any(feature = "replay-mcap", feature = "replay-styxrec")))]
mod corruption_tests;
#[cfg(all(test, feature = "replay-mcap"))]
mod playback_tests;
#[cfg(all(test, feature = "replay-mcap"))]
mod recording_tests;
#[cfg(all(test, any(feature = "replay-mcap", feature = "replay-styxrec")))]
mod tests;
