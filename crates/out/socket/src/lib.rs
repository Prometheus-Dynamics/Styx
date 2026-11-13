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
pub use reader::{Frame, ReadError, Reader, ReaderStats};
pub use shm_ring_holds::Limits;
pub use wire::USER_MAX;

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
        Self::Ring(err)
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
