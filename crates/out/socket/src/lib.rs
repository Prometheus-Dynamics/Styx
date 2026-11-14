//! Frames to other processes over the shared ring.
//!
//! This is the one way a Styx camera service sends frames to other processes without Orion: the
//! service (the producer) takes [`FrameLease`](styx_frame::FrameLease)s from one
//! [`FramePool`](styx_frame::FramePool), and each reader gets the pool's buffers, the ring and a
//! doorbell once, over a Unix socket. After that every frame is one ring entry naming a buffer
//! and carrying the frame's identity, format and planes as opaque bytes. Nothing is copied.
//!
//! - **Pools**: [`Producer::new`] makes its own pool, every buffer at a fixed offset in one
//!   memfd. [`Producer::with_pool`] publishes an existing [`FramePool`] whose buffers are
//!   fd-backed: memfds (sealed against shrinking) or dma-bufs, one descriptor per buffer or
//!   several buffers at offsets in one. Such a pool may hold up to 256 descriptors.
//! - **Geometry**: [`Geometry`] is the one record of the ring's and pool's shape: payload, slack,
//!   buffers and history. The handshake carries it, and the reader builds its frame pool from it.
//! - **Handshake** ([`Producer::accept`], [`Reader::connect`]): the producer sends the offer (the
//!   geometry, the reader's slot and the ring's read end and pool page), then one record per
//!   buffer: its descriptor, memory kind, offset and length. A descriptor is sent with the first
//!   buffer that names it, one per message. The reader maps each buffer read-only as a
//!   `FrameLease` of its own pool, so a dma-buf frame is `MemoryKind::Dmabuf` there and names its
//!   dma-buf for a GPU import. The reader answers with its marks page and its doorbell. The
//!   producer registers the reader's marks before it publishes anything, so no buffer a reader
//!   holds can be reused.
//! - **Start**: [`Reader::connect`] starts at the newest frame the ring holds ([`Start::Newest`]),
//!   delivered at once; [`Reader::connect_with`] with [`Start::NextNew`] starts after the frames
//!   already published. A reader's place is fixed when it answers the handshake.
//! - **Doorbells**: [`Producer::add_bell`] adds a doorbell that is not a socket reader's (an
//!   adopter's, say); every publish rings it with the readers'.
//! - **Adopters**: [`Producer::ring`], [`Producer::pool`], [`Producer::holds`] and
//!   [`Producer::geometry`] borrow the objects an adopter needs; [`Producer::holds_mut`] lends
//!   the hold table for an adopter to add a subscriber's marks.
//! - **Holds**: a reader marks each buffer of a frame before it checks the buffer's ticket
//!   (`shm-ring-holds`). The producer keeps the frame's lease while the ring holds the buffer, and
//!   takes a buffer back only when no reader marks it.
//! - **The producer never waits.** With no free buffer a frame is dropped and counted. A reader
//!   that is too slow loses frames to the ring's history and counts them as skipped.
//! - **Limits**: [`Limits`] caps the buffers one reader may hold and how long it may hold one; a
//!   reader over a limit is cut off, and told so when it next looks.
//! - **Death**: a reader whose socket closes (the process exits, or the reader is dropped) loses
//!   its marks at once. Its buffers come back on the next [`Producer::begin`].
//!
//! Linux only: the ring is a Linux shared-memory layout. Everything here is a safe wrapper over
//! Lemnos's `shm-*` crates and Styx's frame pool, except one `BufferMemory` wrapper in the reader
//! that counts dma-buf sync brackets and only forwards them (its `SAFETY` comment says why).

#![cfg(target_os = "linux")]

mod geometry;
mod producer;
mod reader;
mod wire;

use core::fmt;

pub use geometry::Geometry;
pub use producer::{
    Accepted, BeginError, BellId, HANDSHAKE_TIMEOUT, HandshakePlan, OUTSIDE_BELLS, Producer,
    ProducerConfig, ProducerStats, ReaderInfo,
};
pub use reader::{Frame, ReadError, Reader, ReaderStats, Start};
pub use shm_ring_holds::Limits;
pub use wire::USER_MAX;
/// Why a stream ended. The same four meanings as the hardware layer's stream ends (I2h), so a
/// reader learns one thing whichever ring it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// The producer ended the stream on purpose: a clean stop, the camera removed, the service
    /// going away. Every frame published has been delivered first.
    Closed,
    /// The stream ended because THIS READER broke the rules: a hold kept too long, a bad mark.
    /// The producer cut the reader off for it. It is not a fault of the producer.
    ConsumerFault,
    /// The device failed for good and will not continue on this attachment. Only a producer
    /// whose camera was turned off for good writes it; a transient failure is a stall, not an end.
    DeviceFailed,
    /// Any other end code. The value is kept; the stream is never treated as running.
    Unknown(u32),
}

impl StreamEnd {
    /// The end code the ring holds for this end (`Unknown` keeps its value).
    pub const fn code(self) -> u32 {
        match self {
            Self::Closed => ENDED_CLOSED,
            Self::ConsumerFault => ENDED_CONSUMER_FAULT,
            Self::DeviceFailed => ENDED_DEVICE_FAILED,
            Self::Unknown(code) => code,
        }
    }

    /// The end a ring's end code names; `None` for 0, which means the ring still runs.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => None,
            ENDED_CLOSED => Some(Self::Closed),
            ENDED_CONSUMER_FAULT => Some(Self::ConsumerFault),
            ENDED_DEVICE_FAILED => Some(Self::DeviceFailed),
            other => Some(Self::Unknown(other)),
        }
    }
}

/// The end code of [`StreamEnd::Closed`]: the producer closed its ring cleanly ([`Producer::close`],
/// or its drop).
pub const ENDED_CLOSED: u32 = 1;
/// The end code of [`StreamEnd::ConsumerFault`]. A producer does not write it on its own behalf:
/// a cut-off reader gets this outcome from its own read, not from the ring's end word.
pub const ENDED_CONSUMER_FAULT: u32 = 2;
/// The end code of [`StreamEnd::DeviceFailed`].
pub const ENDED_DEVICE_FAILED: u32 = 3;
/// The handshake version this build speaks. A peer on another version is refused by name
/// ([`SocketError::Version`]).
pub const HANDSHAKE_VERSION: u32 = wire::VERSION;

/// A shared structure whose layout value this build does not read. See [`SocketError::Layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedLayout {
    /// The frame ring's header (`shm_ring::LAYOUT`, word 1).
    Ring,
    /// A reader's marks page (`shm_ring_holds::LAYOUT`).
    Marks,
}

impl SharedLayout {
    /// The structure as a person reads it: the start of the refusal's message.
    #[must_use]
    pub const fn what(self) -> &'static str {
        match self {
            Self::Ring => "this ring",
            Self::Marks => "this marks page",
        }
    }
}

/// Why a handshake, a publish or a connection failed.
#[derive(Debug)]
pub enum SocketError {
    /// A socket call failed.
    Io(std::io::Error),
    /// Sending descriptors or a message over the handshake socket failed.
    Send(shm_fdpass::SendError),
    /// Receiving a handshake message failed.
    Recv(shm_fdpass::RecvError),
    /// The ring, its pool page or a hold page could not be made or attached.
    Ring(shm_ring::RingError),
    /// The ring refused a buffer operation (a frame names a buffer that is not free, and so on).
    Buffer(shm_ring_buffers::BufferError),
    /// The frame pool refused its buffers.
    Pool(styx_frame::PoolError),
    /// A handshake message did not have the shape this build expects.
    Protocol(&'static str),
    /// The configuration asks for something this crate does not allow.
    Config(&'static str),
    /// A shared structure (the ring or a marks page) carries a layout value this build does not
    /// read: the peer is another layout. `found` is the value in the memory, `expected` the one
    /// this build reads. Nothing is mapped or linked when this is returned.
    Layout {
        /// The structure whose layout is refused.
        which: SharedLayout,
        /// The value in the memory.
        found: u64,
        /// The value this build reads.
        expected: u64,
    },
    /// The handshake's version is not this build's: `theirs` is the peer's, `ours` this build's.
    /// Nothing is linked when this is returned.
    Version {
        /// This build's handshake version ([`HANDSHAKE_VERSION`]).
        ours: u32,
        /// The version the peer sent.
        theirs: u32,
    },
    /// Every reader slot the ring has is taken.
    NoReaderSlot,
    /// Every outside doorbell slot ([`OUTSIDE_BELLS`]) is in use.
    NoBellSlot,
}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "socket: {err}"),
            Self::Send(err) => write!(f, "handshake send: {err}"),
            Self::Recv(err) => write!(f, "handshake receive: {err}"),
            Self::Ring(err) => write!(f, "{err}"),
            Self::Buffer(err) => write!(f, "{err}"),
            Self::Pool(err) => write!(f, "{err}"),
            Self::Protocol(why) => write!(f, "handshake: {why}"),
            Self::Layout {
                which,
                found,
                expected,
            } => write!(
                f,
                "{} is layout {found}; this build reads layout {expected}",
                which.what()
            ),
            Self::Version { ours, theirs } => write!(
                f,
                "handshake version {theirs} from the other side; this build speaks version {ours}"
            ),
            Self::Config(why) => write!(f, "configuration: {why}"),
            Self::NoReaderSlot => f.write_str("every reader slot of the ring is taken"),
            Self::NoBellSlot => f.write_str("every outside doorbell slot is in use"),
        }
    }
}

impl std::error::Error for SocketError {}

impl From<std::io::Error> for SocketError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<shm_fdpass::SendError> for SocketError {
    fn from(err: shm_fdpass::SendError) -> Self {
        Self::Send(err)
    }
}

impl From<shm_fdpass::RecvError> for SocketError {
    fn from(err: shm_fdpass::RecvError) -> Self {
        Self::Recv(err)
    }
}

impl From<shm_ring::RingError> for SocketError {
    fn from(err: shm_ring::RingError) -> Self {
        // The shm crate's layout refusal keeps its two values, and the structure it names.
        match err {
            shm_ring::RingError::Layout { what, found, reads } => match what {
                "this ring" => Self::Layout {
                    which: SharedLayout::Ring,
                    found,
                    expected: reads,
                },
                "this marks page" => Self::Layout {
                    which: SharedLayout::Marks,
                    found,
                    expected: reads,
                },
                _ => Self::Ring(shm_ring::RingError::Layout { what, found, reads }),
            },
            other => Self::Ring(other),
        }
    }
}

impl From<shm_ring_buffers::BufferError> for SocketError {
    fn from(err: shm_ring_buffers::BufferError) -> Self {
        Self::Buffer(err)
    }
}

impl From<styx_frame::PoolError> for SocketError {
    fn from(err: styx_frame::PoolError) -> Self {
        Self::Pool(err)
    }
}
