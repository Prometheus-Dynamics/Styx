//! Learning the producer's buffers: the node needs each ring buffer's descriptor before PipeWire
//! can name it, and a reader only sees a buffer's descriptor through a frame in it. So the node
//! first takes frames, duplicates each new index's descriptor, and releases the frame at once.
//! No PipeWire here: this part is tested without the library.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use styx_format::{FourCc, Resolution};
use styx_socket::Reader;

const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// Why the buffers could not be learned.
#[derive(Debug)]
pub enum LearnError {
    /// The stop flag was set first.
    Stopped,
    /// The producer went away while the buffers were learned.
    ProducerLost,
    /// Buffer `index` is not a memfd or dma-buf, so it cannot be handed to PipeWire by reference.
    NoDescriptor(usize),
    /// The frames changed format while the buffers were learned.
    FormatChanged,
    /// Duplicating a descriptor failed.
    Io(io::Error),
}

impl std::fmt::Display for LearnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("stopped"),
            Self::ProducerLost => f.write_str("the producer went away while buffers were learned"),
            Self::NoDescriptor(index) => {
                write!(f, "buffer {index} is not a memfd or dma-buf")
            }
            Self::FormatChanged => {
                f.write_str("the producer changed format while buffers were learned")
            }
            Self::Io(err) => write!(f, "duplicating a buffer descriptor: {err}"),
        }
    }
}

impl std::error::Error for LearnError {}

/// The producer's buffers, as PipeWire will use them: one descriptor and length per ring buffer,
/// and the frame format every buffer carries.
#[derive(Debug)]
pub struct Buffers {
    /// Ring buffer `i`'s memfd (a duplicate: the producer's own is not ours to close).
    pub fds: Vec<OwnedFd>,
    /// Ring buffer `i`'s length in bytes.
    pub lens: Vec<u32>,
    pub fourcc: FourCc,
    pub resolution: Resolution,
    /// Row stride of plane 0.
    pub stride: u32,
    /// Bytes of one frame (the planes' span).
    pub frame_len: u32,
}

/// Connects a reader to the producer at `socket`, trying again with backoff (100 ms doubling to
/// 5 s) until it connects or `stop` is set.
pub fn connect(socket: &Path, stop: &AtomicBool) -> Option<Reader> {
    let mut backoff = BACKOFF_START;
    loop {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        let connected = UnixStream::connect(socket)
            .ok()
            .and_then(|stream| Reader::connect(stream).ok());
        if let Some(reader) = connected {
            return Some(reader);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_CAP);
    }
}

/// Learns the descriptor of each of `buffers` ring buffers from the frames `reader` delivers.
/// Frames are released as soon as their descriptor is taken, so the producer keeps running.
///
/// # Errors
///
/// [`LearnError`] when stopped, when the producer goes away, when a buffer has no descriptor, or
/// when the format changes.
pub fn learn(
    reader: &mut Reader,
    buffers: usize,
    stop: &AtomicBool,
) -> Result<Buffers, LearnError> {
    let mut found: Vec<Option<(OwnedFd, u32)>> = (0..buffers).map(|_| None).collect();
    let mut count = 0;
    let mut shape: Option<(FourCc, Resolution, u32, u32)> = None;
    while count < buffers {
        if stop.load(Ordering::Acquire) {
            return Err(LearnError::Stopped);
        }
        match reader.next_frame() {
            Ok(Some(frame)) => {
                let lease = frame.lease();
                let format = lease.format();
                let stride = lease
                    .layouts()
                    .first()
                    .map_or(0, |plane| plane.stride as u32);
                let frame_len = lease.layouts().span().unwrap_or(0) as u32;
                let this = (format.code, format.resolution, stride, frame_len);
                match shape {
                    None => shape = Some(this),
                    Some(known) if known != this => return Err(LearnError::FormatChanged),
                    Some(_) => {}
                }
                let index = frame.buffer();
                if index < buffers && found[index].is_none() {
                    let memory = lease.memory();
                    let fd = memory.fd().ok_or(LearnError::NoDescriptor(index))?;
                    let owned = fd.try_clone_to_owned().map_err(LearnError::Io)?;
                    found[index] = Some((owned, memory.len() as u32));
                    count += 1;
                }
            }
            Ok(None) => {
                let _ = reader.wait(Some(Duration::from_millis(20)));
            }
            Err(_) => return Err(LearnError::ProducerLost),
        }
    }
    let (fourcc, resolution, stride, frame_len) = shape.ok_or(LearnError::ProducerLost)?;
    let (fds, lens) = found
        .into_iter()
        .map(|entry| entry.expect("every index was learned"))
        .unzip();
    Ok(Buffers {
        fds,
        lens,
        fourcc,
        resolution,
        stride,
        frame_len,
    })
}

impl Buffers {
    /// The raw descriptors, for handing to the PipeWire shim (the shim duplicates them).
    pub fn raw_fds(&self) -> Vec<i32> {
        self.fds
            .iter()
            .map(|fd| std::os::fd::AsRawFd::as_raw_fd(&fd.as_fd()))
            .collect()
    }
}
