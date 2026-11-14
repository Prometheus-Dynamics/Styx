//! One reader of a camera's frame socket: connects, takes frames, decodes their metadata, runs
//! the optional ISP, and keeps the counts. [`Session::run`] is the read loop.

use std::fmt;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use styx_controls::FrameMetadata;
use styx_socket::{
    Carried, Frame, Next, ProducerState, ReadError, Reader, ReaderStats, SocketError, Start,
};

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
    /// The handshake or the buffer table failed; the socket's own error, typed (a layout or a
    /// version refusal names its values).
    Handshake(SocketError),
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
            SessionError::Handshake(e) => write!(f, "handshake: {e}"),
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
    /// Producer stall edges seen: the camera retried, and came back. Each is printed when seen.
    pub stalls: u64,
    /// Producer resume edges seen (the camera running again after a stall this reader was told of).
    pub resumes: u64,
    /// Resumes that were a device rebind (the same stall may end `Lost`), each a discontinuity of
    /// the sequence.
    pub lost_resumes: u64,
    /// When the stall in force began (the monotonic time its edge was seen), while one is.
    stall_started: Option<u64>,
    /// The time the camera spent stalled, summed over the stalls seen, in ns.
    pub stall_ns_total: u64,
}

impl Session {
    /// How long without a frame before the loop stops.
    pub const IDLE: Duration = Duration::from_secs(30);

    /// Connects to the camera's frame socket at `socket` and takes the producer's handshake.
    pub fn connect(socket: &Path, isp: Option<IspRunner>) -> Result<Self, SessionError> {
        Self::connect_with(socket, isp, Start::default())
    }

    /// As [`Session::connect`], with the reader's start chosen: the newest kept frame at once,
    /// or only frames published after the attach.
    pub fn connect_with(
        socket: &Path,
        isp: Option<IspRunner>,
        start: Start,
    ) -> Result<Self, SessionError> {
        let stream = UnixStream::connect(socket).map_err(|error| SessionError::Connect {
            socket: socket.to_path_buf(),
            error,
        })?;
        let reader = Reader::connect_with(stream, start).map_err(SessionError::Handshake)?;
        let reader_start = reader.stats();
        Ok(Self {
            socket: socket.to_path_buf(),
            reader,
            stats: FrameStats::new(),
            metadata: None,
            isp,
            reader_start,
            stalls: 0,
            resumes: 0,
            lost_resumes: 0,
            stall_started: None,
            stall_ns_total: 0,
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
            match self.reader.next_event().map_err(SessionError::Read)? {
                Next::Frame(frame) => {
                    last_frame = monotonic_ns();
                    self.handle(&frame)?;
                    if done(&self.stats) {
                        return Ok(Stop::Frames);
                    }
                }
                Next::Stalled(state) => self.stall_edge(state),
                Next::Resumed { state } => self.resume_edge(state),
                Next::Idle => {
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

    /// A stall edge: the camera retries. Printed with the time it was seen and the time it began.
    fn stall_edge(&mut self, state: ProducerState) {
        self.stalls += 1;
        self.stall_started.get_or_insert_with(monotonic_ns);
        let since = match state {
            ProducerState::Retrying { since_ns } => since_ns,
            _ => 0,
        };
        eprintln!(
            "producer stalled: seen at {} ns, retrying since {since} ns (edge {})",
            monotonic_ns(),
            self.stalls
        );
    }

    /// A resume edge: the camera runs again. Printed with the time it was seen and its kind.
    fn resume_edge(&mut self, state: Carried) {
        self.resumes += 1;
        let now = monotonic_ns();
        let stalled_ns = self
            .stall_started
            .take()
            .map_or(0, |start| now.saturating_sub(start));
        self.stall_ns_total += stalled_ns;
        let lost = matches!(state, Carried::Lost);
        if lost {
            self.lost_resumes += 1;
        }
        // The frames after the stall are a new run of ids, or go on from the last one: either way the
        // stall is not a gap in them (see `FrameStats::across_stall`).
        self.stats.across_stall();
        eprintln!(
            "producer resumed: seen at {now} ns, {} (edge {}), stalled {} ms",
            if lost {
                "lost: rebound, a new sequence"
            } else {
                "kept: same driver instance"
            },
            self.resumes,
            stalled_ns as f64 / 1e6
        );
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
