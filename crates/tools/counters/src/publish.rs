//! The counters ring: each snapshot is one buffer on a `styx-socket` ring, so a reader in any
//! process reads it the way it reads frames (connect, then take entries). The publisher never
//! waits for a reader: a snapshot with no free buffer is dropped and counted.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, packed_layouts};
use styx_socket::{
    Accepted, BeginError, HANDSHAKE_TIMEOUT, Limits, Producer, ProducerConfig, Reader, SocketError,
};

use crate::snapshot::{SNAPSHOT_BYTES, Snapshot, SnapshotError, read_snapshot};

/// Buffers on the counters ring: snapshots a reader may still be holding when the next is taken.
const BUFFERS: usize = 16;
/// Ring entries kept: a reader that falls this far behind skips snapshots (it reads the newest).
const HISTORY: u32 = 16;
/// What each reader may hold at once. Snapshots are read and released at once.
const READER_LIMITS: Limits = Limits {
    max_held: 4,
    max_hold: None,
};

/// Why the counters ring could not be used.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Socket(SocketError),
    Begin(BeginError),
    Snapshot(SnapshotError),
    /// The reader's frame had no bytes to read (a ring of another shape).
    Empty,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "counters: {e}"),
            Error::Socket(e) => write!(f, "counters: {e}"),
            Error::Begin(e) => write!(f, "counters: {e}"),
            Error::Snapshot(e) => write!(f, "counters: {e}"),
            Error::Empty => f.write_str("counters: an entry without bytes"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<SocketError> for Error {
    fn from(e: SocketError) -> Self {
        Error::Socket(e)
    }
}

impl From<SnapshotError> for Error {
    fn from(e: SnapshotError) -> Self {
        Error::Snapshot(e)
    }
}

/// The publishing side: a listener for readers, and the ring the snapshots go on.
pub struct Publisher {
    listener: UnixListener,
    path: PathBuf,
    producer: Producer<()>,
    media: MediaFormat,
    layouts: Layouts,
    /// Readers whose handshake is running on its own thread, and the finished ones.
    pending: Receiver<Result<Accepted, SocketError>>,
    done: Sender<Result<Accepted, SocketError>>,
    /// Snapshots published, and snapshots the ring had no buffer for.
    pub published: u64,
    pub dropped: u64,
}

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("path", &self.path)
            .field("published", &self.published)
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

impl Publisher {
    /// Binds `path` for readers and makes the ring. Replaces a socket file left at `path`.
    pub fn bind(path: &Path) -> Result<Self, Error> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let producer = Producer::new(ProducerConfig {
            payload: SNAPSHOT_BYTES,
            buffers: BUFFERS,
            history: HISTORY,
        })?;
        let media = MediaFormat::with_defaults(
            FourCc::GREY,
            Resolution::new(SNAPSHOT_BYTES as u32, 1).ok_or(Error::Empty)?,
        );
        let layouts = packed_layouts(FourCc::GREY, SNAPSHOT_BYTES as u32, 1, None, None)
            .map_err(|_| Error::Empty)?;
        let (done, pending) = mpsc::channel();
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            producer,
            media,
            layouts,
            pending,
            done,
            published: 0,
            dropped: 0,
        })
    }

    /// The socket readers connect to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Readers connected now.
    pub fn readers(&self) -> usize {
        self.producer.stats().readers
    }

    /// Accepts readers that connected since the last call, and attaches those whose handshake
    /// has finished. A reader's handshake runs on its own thread, so a stuck one never holds up
    /// the publisher; it is dropped at [`HANDSHAKE_TIMEOUT`].
    pub fn accept(&mut self) -> Result<(), Error> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => self.start_handshake(stream)?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        loop {
            match self.pending.try_recv() {
                Ok(Ok(accepted)) => {
                    // A reader that cannot be attached (no slot left) is closed; it connects again.
                    let _ = self.producer.attach(accepted);
                }
                Ok(Err(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        Ok(())
    }

    fn start_handshake(&mut self, stream: UnixStream) -> Result<(), Error> {
        stream.set_nonblocking(false)?;
        let plan = self.producer.handshake_plan()?;
        let done = self.done.clone();
        std::thread::Builder::new()
            .name("styx-counters-handshake".to_string())
            .spawn(move || {
                let _ = done.send(plan.run(stream, READER_LIMITS, HANDSHAKE_TIMEOUT));
            })?;
        Ok(())
    }

    /// Publishes one snapshot: one buffer, one entry. Returns `false` (and counts it) when the
    /// ring has no free buffer for it.
    pub fn publish(&mut self, snapshot: &Snapshot) -> Result<bool, Error> {
        let bytes = snapshot.to_bytes();
        let entry = FrameEntry::new(snapshot.seq, snapshot.taken_ns, self.media, self.layouts);
        let mut lease = match self.producer.begin(entry) {
            Ok(lease) => lease,
            Err(BeginError::NoFreeBuffer) => {
                self.dropped += 1;
                self.producer.reclaim();
                return Ok(false);
            }
            Err(e) => return Err(Error::Begin(e)),
        };
        {
            let mut planes = lease.write().ok_or(Error::Empty)?;
            let plane = planes.plane(0).ok_or(Error::Empty)?;
            plane.data[..SNAPSHOT_BYTES].copy_from_slice(&bytes);
        }
        self.producer.publish_with(lease, &[])?;
        self.producer.reclaim();
        self.published += 1;
        Ok(true)
    }
}

/// The reading side, for any process: connects to a counters ring and reads snapshots.
pub struct Subscriber {
    reader: Reader,
}

impl std::fmt::Debug for Subscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscriber").finish_non_exhaustive()
    }
}

impl Subscriber {
    /// Connects to the counters ring at `path`.
    pub fn connect(path: &Path) -> Result<Self, Error> {
        let stream = UnixStream::connect(path)?;
        Ok(Self {
            reader: Reader::connect(stream)?,
        })
    }

    /// The next snapshot, or `None` when none is waiting. Never waits.
    pub fn next_snapshot(&mut self) -> Result<Option<Snapshot>, Error> {
        let Some(frame) = self
            .reader
            .next_frame()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?
        else {
            return Ok(None);
        };
        let lease = frame.lease();
        let read = lease.read().ok_or(Error::Empty)?;
        let plane = read.plane(0).ok_or(Error::Empty)?;
        Ok(Some(read_snapshot(plane.data)?))
    }

    /// Waits up to `timeout` for a snapshot to arrive.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        self.reader.wait(Some(timeout)).map(|_| ())
    }

    /// The reader's own counts (its skipped snapshots among them).
    pub fn stats(&self) -> styx_socket::ReaderStats {
        self.reader.stats()
    }
}
