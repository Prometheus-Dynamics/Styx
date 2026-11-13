//! Learning the producer's buffers: the node needs each ring buffer's descriptor before PipeWire
//! can name it, and a reader only sees a buffer's descriptor through a frame in it. The learner
//! takes frames one at a time, duplicates each new index's descriptor, and the caller releases
//! each frame at once, so the producer keeps running while the node learns. No PipeWire here.

use std::io;
use std::os::fd::{AsFd, OwnedFd};

use styx_format::{FourCc, Resolution};
use styx_socket::Frame;

/// Why the buffers could not be learned. The node treats each as a lost producer and retries.
#[derive(Debug)]
pub enum LearnError {
    /// Buffer `index` is not a memfd or dma-buf, so it cannot be handed to PipeWire by reference.
    NoDescriptor(usize),
    /// The frames changed format while the buffers were learned.
    FormatChanged,
    /// Duplicating a descriptor failed.
    Io(io::Error),
    /// [`Learner::finish`] was called before every buffer was seen.
    Incomplete,
}

impl std::fmt::Display for LearnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDescriptor(index) => {
                write!(f, "buffer {index} is not a memfd or dma-buf")
            }
            Self::FormatChanged => {
                f.write_str("the producer changed format while buffers were learned")
            }
            Self::Io(err) => write!(f, "duplicating a buffer descriptor: {err}"),
            Self::Incomplete => f.write_str("not every buffer was learned"),
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
    /// The raw descriptors of `fds`, in order, for the shim's copy.
    raw: Vec<i32>,
}

impl Buffers {
    /// The raw descriptors, in buffer order. The shim duplicates them before the call returns.
    pub fn raw_fds(&self) -> &[i32] {
        &self.raw
    }
}

/// The shape every frame of one producer carries. A change in it is a new producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    fourcc: FourCc,
    resolution: Resolution,
    stride: u32,
    frame_len: u32,
}

/// Learns the descriptors of `buffers` ring buffers, one frame at a time.
#[derive(Debug)]
pub struct Learner {
    found: Vec<Option<(OwnedFd, u32)>>,
    count: usize,
    shape: Option<Shape>,
}

impl Learner {
    /// A learner for a producer with `buffers` ring buffers (from its handshake's geometry).
    pub fn new(buffers: usize) -> Self {
        Self {
            found: (0..buffers).map(|_| None).collect(),
            count: 0,
            shape: None,
        }
    }

    /// Takes `frame`'s buffer descriptor when its index is new. Returns `true` once every buffer
    /// is known. The caller drops the frame: the descriptor it took is a duplicate.
    ///
    /// # Errors
    ///
    /// [`LearnError`] when the frame's buffer has no descriptor, the format changed, or a
    /// duplicate fails.
    pub fn feed(&mut self, frame: &Frame) -> Result<bool, LearnError> {
        let lease = frame.lease();
        let format = lease.format();
        let stride = lease
            .layouts()
            .first()
            .map_or(0, |plane| plane.stride as u32);
        let frame_len = lease.layouts().span().unwrap_or(0) as u32;
        let this = Shape {
            fourcc: format.code,
            resolution: format.resolution,
            stride,
            frame_len,
        };
        match self.shape {
            None => self.shape = Some(this),
            Some(known) if known != this => return Err(LearnError::FormatChanged),
            Some(_) => {}
        }
        let index = frame.buffer();
        if index < self.found.len() && self.found[index].is_none() {
            let memory = lease.memory();
            let fd = memory.fd().ok_or(LearnError::NoDescriptor(index))?;
            let owned = fd.try_clone_to_owned().map_err(LearnError::Io)?;
            self.found[index] = Some((owned, memory.len() as u32));
            self.count += 1;
        }
        Ok(self.count == self.found.len())
    }

    /// The buffers, once every one is known.
    ///
    /// # Errors
    ///
    /// [`LearnError::Incomplete`] when some buffer was not seen.
    pub fn finish(self) -> Result<Buffers, LearnError> {
        let shape = match (self.count == self.found.len(), self.shape) {
            (true, Some(shape)) => shape,
            _ => return Err(LearnError::Incomplete),
        };
        let (fds, lens): (Vec<OwnedFd>, Vec<u32>) = self.found.into_iter().flatten().unzip();
        let raw = fds
            .iter()
            .map(|fd| std::os::fd::AsRawFd::as_raw_fd(&fd.as_fd()))
            .collect();
        Ok(Buffers {
            fds,
            lens,
            fourcc: shape.fourcc,
            resolution: shape.resolution,
            stride: shape.stride,
            frame_len: shape.frame_len,
            raw,
        })
    }
}
