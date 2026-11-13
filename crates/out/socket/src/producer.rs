//! The producer: the camera service's end of the shared ring.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use shm_fdpass::{self as fdpass, MAX_FDS};
use shm_ring::{FieldId, ReadMap, Ring, SampleTime, Spec, WriteMap};
use shm_ring_buffers::{BufferDesc, BufferIndex, BufferWriter, Dropped, MAX_BUFFERS, Pool, entry};
use shm_ring_doorbell::Bell;
use shm_ring_holds::{HoldTable, Limits};
use styx_frame::{
    AcquireError, BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool,
};

use crate::SocketError;
use crate::wire::{self, ACK_LEN, CHUNK_LEN, META_MAX, OFFER_LEN, VERSION};

/// Descriptors the shared ring's buffer topic can name: its descriptor numbers are a byte.
const MAX_DESCRIPTORS: usize = 256;
/// How long a reader has to answer the handshake.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);
/// The ring's field carrying the frames.
const FIELD: FieldId = FieldId(0);

/// The shape of a producer's ring and buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerConfig {
    /// Bytes in each buffer. Frames may use all of them.
    pub payload: usize,
    /// Buffers in the pool: one memfd each. At most 256.
    pub buffers: usize,
    /// Ring entries kept. A reader that falls further behind than this loses frames.
    pub history: u32,
}

/// Why [`Producer::begin`] gave no frame. Nothing was published; the frame is dropped and counted.
#[derive(Debug)]
pub enum BeginError {
    /// Every buffer is published and at least one of the rest is held by a reader.
    NoFreeBuffer,
    /// The pool refused the buffer the ring gave (a frame longer than the buffer).
    Frame(AcquireError),
}

impl core::fmt::Display for BeginError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoFreeBuffer => f.write_str("no buffer is free; the frame is dropped"),
            Self::Frame(err) => write!(f, "frame refused: {err}"),
        }
    }
}

impl std::error::Error for BeginError {}

/// One connected reader.
#[derive(Debug)]
struct Link {
    slot: u32,
    socket: UnixStream,
    bell: Bell,
}

/// Counts for a producer, for reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProducerStats {
    /// Frames published.
    pub published: u64,
    /// Frames dropped for want of a free buffer.
    pub dropped: u64,
    /// Readers connected now.
    pub readers: usize,
    /// Readers cut off for breaking their limits.
    pub cut_off: usize,
    /// Readers whose socket closed (process exit or drop); their holds were removed.
    pub gone: u64,
}

/// One connected reader, for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderInfo {
    /// The reader's slot in the ring.
    pub slot: u32,
    /// Whether the reader has been cut off for its limits.
    pub cut_off: bool,
}

/// Publishes frames to any number of readers in other processes.
///
/// Hold it on one thread: [`Producer::begin`] and [`Producer::publish`] are the frame path and
/// make no system call beyond one eventfd write per reader that is asleep.
#[derive(Debug)]
pub struct Producer {
    ring: Ring,
    writer: BufferWriter<WriteMap>,
    frames: FramePool<()>,
    holds: HoldTable<ReadMap>,
    links: Vec<Link>,
    /// For each buffer, the lease on the frame the ring still holds in it. Kept until the ring
    /// takes the buffer back, so the buffer stays this producer's while any reader may read it.
    in_flight: Vec<Option<FrameLease<()>>>,
    payload: usize,
    published: u64,
    dropped: u64,
    gone: u64,
}

impl Producer {
    /// A producer with `config`'s ring and buffers, all free and no readers yet.
    ///
    /// # Errors
    ///
    /// [`SocketError`] if the shape is not allowed or the memory can't be made.
    pub fn new(config: ProducerConfig) -> Result<Self, SocketError> {
        let ProducerConfig {
            payload,
            buffers,
            history,
        } = config;
        if payload == 0 {
            return Err(SocketError::Config("a buffer holds at least one byte"));
        }
        if buffers == 0 || buffers > MAX_BUFFERS as usize {
            return Err(SocketError::Config("a pool holds 1 to 1024 buffers"));
        }
        if buffers > MAX_DESCRIPTORS {
            return Err(SocketError::Config(
                "one memfd per buffer: at most 256 buffers (the ring's descriptor numbers are a byte)",
            ));
        }
        let ring = Ring::create(&Spec {
            fields: 1,
            history,
            max_payload: entry::len(1, META_MAX) as u32,
            expiry_ms: 0,
        })?;
        let mut memories: Vec<Box<dyn BufferMemory>> = Vec::with_capacity(buffers);
        let mut descriptors: Vec<OwnedFd> = Vec::with_capacity(buffers);
        for _ in 0..buffers {
            let fd = shm_ring::sealed_memfd(payload)?;
            descriptors.push(fd.try_clone()?);
            memories.push(Box::new(FdMemory::memfd(fd, payload, true)?));
        }
        let frames: FramePool<()> = FramePool::new(BufferSize::new(payload, 0), memories)?;
        let buffer_descs: Vec<BufferDesc> = (0..buffers)
            .map(|index| BufferDesc {
                descriptor: index as u8,
                offset: 0,
                size: payload as u64,
            })
            .collect();
        let pool = Pool::new(1, descriptors, buffer_descs)?;
        let writer = BufferWriter::new(ring.writer()?, FIELD, pool);
        Ok(Self {
            ring,
            writer,
            frames,
            holds: HoldTable::new(buffers as u32),
            links: Vec::new(),
            in_flight: (0..buffers).map(|_| None).collect(),
            payload,
            published: 0,
            dropped: 0,
            gone: 0,
        })
    }

    /// Hands a connected reader the ring and the buffers, then registers the marks and the
    /// doorbell it sends back. Returns the reader's slot. Blocks for the reader's reply, at most
    /// five seconds; call it from the control path, never from the frame path.
    ///
    /// The reader's marks are registered before this returns, so no frame is published to it
    /// before its holds are visible.
    ///
    /// # Errors
    ///
    /// [`SocketError`] if the reader does not answer as expected or every slot is taken.
    pub fn accept(&mut self, socket: UnixStream, limits: Limits) -> Result<u32, SocketError> {
        let slot = self.holds.free_slot().ok_or(SocketError::NoReaderSlot)?;
        socket.set_read_timeout(Some(ANSWER_WITHIN))?;
        let ring_fd = self.ring.read_fd()?;
        let page_fd = self.writer.pool().page_fd()?;
        let descriptors: Vec<BorrowedFd<'_>> = self.writer.pool().descriptors().collect();
        let total = descriptors.len();

        let mut offer = [0_u8; OFFER_LEN];
        offer[0..4].copy_from_slice(&VERSION.to_le_bytes());
        offer[4..8].copy_from_slice(&(total as u32).to_le_bytes());
        offer[8..12].copy_from_slice(&slot.to_le_bytes());
        offer[12..16].copy_from_slice(&limits.max_held.to_le_bytes());
        offer[16..24].copy_from_slice(&(self.payload as u64).to_le_bytes());
        let first = total.min(MAX_FDS - 2);
        let mut fds = vec![ring_fd.as_fd(), page_fd.as_fd()];
        fds.extend_from_slice(&descriptors[..first]);
        fdpass::send_on(&socket, &[&offer], &fds)?;
        drop(fds);

        let mut next = first;
        while next < total {
            let count = (total - next).min(MAX_FDS);
            let mut chunk = [0_u8; CHUNK_LEN];
            chunk[0..4].copy_from_slice(&VERSION.to_le_bytes());
            chunk[4..8].copy_from_slice(&(next as u32).to_le_bytes());
            fdpass::send_on(&socket, &[&chunk], &descriptors[next..next + count])?;
            next += count;
        }

        let mut ack = [0_u8; ACK_LEN];
        let received = fdpass::recv(socket.as_fd(), &mut ack, 3, false)?.ok_or(
            SocketError::Protocol("the reader closed before it answered"),
        )?;
        if received.len != ACK_LEN
            || ack[0..4] != VERSION.to_le_bytes()
            || ack[4..8] != slot.to_le_bytes()
        {
            return Err(SocketError::Protocol(
                "the reader's answer is not the one asked for",
            ));
        }
        let mut fds = received.fds;
        let (Some(marks), Some(eventfd), Some(bell_page)) = (fds.take(0), fds.take(1), fds.take(2))
        else {
            return Err(SocketError::Protocol(
                "the reader's answer lacks a marks page or doorbell",
            ));
        };
        self.holds.add(slot, marks, limits)?;
        let bell = Bell::attach(eventfd, bell_page)?;
        socket.set_read_timeout(None)?;
        self.links.push(Link { slot, socket, bell });
        Ok(slot)
    }

    /// Takes a free buffer for `entry`'s frame and returns its lease, to be filled and passed to
    /// [`Producer::publish`]. Memory reads and writes only; the producer never waits. With no
    /// buffer free the frame is dropped and counted.
    ///
    /// # Errors
    ///
    /// [`BeginError::NoFreeBuffer`] when every buffer is published or held by a reader.
    pub fn begin(&mut self, entry: FrameEntry) -> Result<FrameLease<()>, BeginError> {
        self.reap();
        let now = SampleTime::now().as_nanos();
        self.holds.enforce(now, self.writer.pool().page());
        let buffer = match self.writer.take(&self.holds) {
            Ok(buffer) => buffer,
            Err(Dropped) => {
                self.dropped += 1;
                return Err(BeginError::NoFreeBuffer);
            }
        };
        // The ring took the buffer back, so the lease on its last frame is no longer needed.
        self.in_flight[buffer.0 as usize] = None;
        self.frames
            .acquire_at(buffer.0 as usize, entry)
            .map_err(|err| {
                self.dropped += 1;
                BeginError::Frame(err)
            })
    }

    /// Publishes a frame whose lease came from [`Producer::begin`] and has been filled. Writes one
    /// ring entry and rings each sleeping reader's doorbell once. The lease is kept until the
    /// ring takes the buffer back.
    ///
    /// # Errors
    ///
    /// [`SocketError::Buffer`] if the ring refuses the buffer. Nothing is published.
    pub fn publish(&mut self, lease: FrameLease<()>) -> Result<u64, SocketError> {
        let index = lease.buffer_index();
        let entry = *lease.entry();
        let mut meta = [0_u8; META_MAX];
        let len = wire::encode_meta(&entry, &mut meta);
        let seq = self.writer.publish(
            &[BufferIndex(index as u32)],
            SampleTime::from_nanos(entry.timestamp_ns),
            &meta[..len],
        )?;
        self.in_flight[index] = Some(lease);
        self.published += 1;
        for link in &mut self.links {
            link.bell.ring();
        }
        Ok(seq)
    }

    /// The payload size of each buffer.
    pub const fn payload(&self) -> usize {
        self.payload
    }

    /// Counts, now.
    pub fn stats(&self) -> ProducerStats {
        ProducerStats {
            published: self.published,
            dropped: self.dropped,
            readers: self.links.len(),
            cut_off: self
                .links
                .iter()
                .filter(|link| self.holds.is_cut_off(link.slot))
                .count(),
            gone: self.gone,
        }
    }

    /// Each connected reader's slot and whether it has been cut off.
    pub fn readers(&self) -> Vec<ReaderInfo> {
        self.links
            .iter()
            .map(|link| ReaderInfo {
                slot: link.slot,
                cut_off: self.holds.is_cut_off(link.slot),
            })
            .collect()
    }

    /// Drops the holds of every reader whose socket has closed. A reader that dies while it
    /// holds frames leaves its marks in its page; this removes them, so the buffers come back.
    fn reap(&mut self) {
        let mut index = 0;
        while index < self.links.len() {
            if peer_closed(&self.links[index].socket) {
                let link = self.links.swap_remove(index);
                self.holds.remove(link.slot, self.writer.pool().page());
                self.gone += 1;
            } else {
                index += 1;
            }
        }
    }
}

/// Whether the peer has closed its end. A reader never sends after its answer, so anything it
/// might send is left unread here; this only looks for end of stream.
fn peer_closed(socket: &UnixStream) -> bool {
    matches!(fdpass::recv(socket.as_fd(), &mut [], 0, true), Ok(None))
}
