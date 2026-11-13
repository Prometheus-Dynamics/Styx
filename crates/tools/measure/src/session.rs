//! One reader of a camera's frame socket: connects, takes frames, decodes their metadata, runs
//! the optional ISP, and keeps the counts. [`Session::run`] is the read loop.

use std::fmt;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use styx_controls::FrameMetadata;
use styx_socket::{Frame, ReadError, Reader, ReaderStats};

use crate::clock::monotonic_ns;
use crate::isp::{IspError, IspRunner};
use crate::stats::FrameStats;

/// How long the loop waits for a frame before it asks the socket again.
const POLL: Duration = Duration::from_millis(50);

/// Why a session ended or failed.
#[derive(Debug)]
pub enum SessionError {
    /// The frame socket could not be reached.
    Connect { socket: PathBuf, error: io::Error },
    /// The handshake or the buffer table failed.
    Handshake(String),
    /// The producer cut this reader off, or replaced its pool.
    Read(ReadError),
    /// The wait for frames failed.
    Wait(io::Error),
    /// The soft ISP failed to set up (a frame is not processed).
    Isp(IspError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Connect { socket, error } => {
                write!(f, "connect {}: {error}", socket.display())
            }
            SessionError::Handshake(why) => write!(f, "handshake: {why}"),
            SessionError::Read(e) => write!(f, "{e}"),
            SessionError::Wait(e) => write!(f, "waiting for frames: {e}"),
            SessionError::Isp(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Why [`Session::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The frame count asked for was reached.
    Frames,
    /// The time asked for ran out.
    Time,
    /// No frame came for [`Session::IDLE`]: the camera stopped or left.
    Idle,
}

/// A reader of one camera's frames, with its measurements.
#[derive(Debug)]
pub struct Session {
    socket: PathBuf,
    reader: Reader,
    pub stats: FrameStats,
    pub metadata: Option<FrameMetadata>,
    isp: Option<IspRunner>,
    /// The ring's frame counts when the session started, so the report can show this run only.
    reader_start: ReaderStats,
}

impl Session {
    /// How long without a frame before the loop stops.
    pub const IDLE: Duration = Duration::from_secs(30);

    /// Connects to the camera's frame socket at `socket` and takes the producer's handshake.
    pub fn connect(socket: &Path, isp: Option<IspRunner>) -> Result<Self, SessionError> {
        let stream = UnixStream::connect(socket).map_err(|error| SessionError::Connect {
            socket: socket.to_path_buf(),
            error,
        })?;
        let reader = Reader::connect(stream).map_err(|e| SessionError::Handshake(e.to_string()))?;
        let reader_start = reader.stats();
        Ok(Self {
            socket: socket.to_path_buf(),
            reader,
            stats: FrameStats::new(),
            metadata: None,
            isp,
            reader_start,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The reader's counts for this session (the ring's skips and holds).
    pub fn reader_stats(&self) -> ReaderStats {
        self.reader.stats()
    }

    /// The reader's counts when the session started.
    pub fn reader_start(&self) -> ReaderStats {
        self.reader_start
    }

    /// The ISP's counts, when one runs.
    pub fn isp_stats(&self) -> Option<crate::stats::IspStats> {
        self.isp.as_ref().map(|isp| isp.stats())
    }

    /// Takes frames until `done` says stop, the time runs out, or no frame comes for
    /// [`Self::IDLE`]. `done` sees the counts after each frame.
    pub fn run(
        &mut self,
        time: Option<Duration>,
        mut done: impl FnMut(&FrameStats) -> bool,
    ) -> Result<Stop, SessionError> {
        let started = monotonic_ns();
        let mut last_frame = started;
        loop {
            match self.reader.next_frame().map_err(SessionError::Read)? {
                Some(frame) => {
                    last_frame = monotonic_ns();
                    self.handle(&frame)?;
                    if done(&self.stats) {
                        return Ok(Stop::Frames);
                    }
                }
                None => {
                    let now = monotonic_ns();
                    if let Some(limit) = time
                        && now.saturating_sub(started) >= limit.as_nanos() as u64
                    {
                        return Ok(Stop::Time);
                    }
                    if now.saturating_sub(last_frame) >= Self::IDLE.as_nanos() as u64 {
                        return Ok(Stop::Idle);
                    }
                    self.reader.wait(Some(POLL)).map_err(SessionError::Wait)?;
                }
            }
        }
    }

    /// One frame in hand: its times and metadata are read, then the ISP runs on it. The frame
    /// goes back to the producer when `frame` drops at the end of the caller's step.
    fn handle(&mut self, frame: &Frame) -> Result<(), SessionError> {
        let now = monotonic_ns();
        let lease = frame.lease();
        // Version 1 and version 2 metadata both read; anything else is refused and counted.
        let meta = FrameMetadata::from_bytes(frame.user());
        let metadata_ok = meta.is_some();
        if let Some(meta) = meta {
            self.metadata = Some(meta);
        }
        self.stats
            .record(lease.id(), lease.timestamp_ns(), now, metadata_ok);
        if let Some(isp) = self.isp.as_mut() {
            // A stage error is counted by the ISP; the frame is not processed and the run goes on.
            if let Err(e) = isp.process(lease)
                && matches!(e, IspError::Setup(_) | IspError::Pool(_))
            {
                return Err(SessionError::Isp(e));
            }
        }
        Ok(())
    }
}
