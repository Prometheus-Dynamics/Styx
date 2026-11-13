//! The producer: the camera service's end of the shared ring.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use shm_fdpass as fdpass;
use shm_ring::{FieldId, ReadMap, Ring, SampleTime, WriteMap};
use shm_ring_buffers::{BufferIndex, BufferWriter, Dropped, Pool};
use shm_ring_doorbell::Bell;
use shm_ring_holds::{HoldTable, Limits};
use styx_frame::{
    AcquireError, BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool,
};

use crate::SocketError;
use crate::geometry::Geometry;
use crate::wire::{ACK_LEN, META_MAX, Offer, VERSION, encode_meta, encode_offer};

/// How long a reader has to answer the handshake.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);
/// The ring's field carrying the frames.
const FIELD: FieldId = FieldId(0);
/// Doorbells a producer can ring besides its socket readers' (see [`Producer::add_bell`]). Its
/// set is allocated in [`Producer::new`]; adding past this is refused, never grown.
pub const OUTSIDE_BELLS: usize = 16;

/// The shape of a producer's ring and buffers, as asked for. [`Producer::geometry`] is the
/// resolved shape the producer runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerConfig {
    /// Bytes in each buffer. Frames may use all of them.
    pub payload: usize,
    /// Buffers in the pool. They all sit in one memfd at fixed offsets. At most 1024.
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

/// A doorbell the producer rings on every publish, from [`Producer::add_bell`]. Used to remove
/// it. A stale id (already removed, or its slot reused) does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BellId {
    slot: u32,
    generation: u32,
}

/// One connected reader.
#[derive(Debug)]
struct Link {
    slot: u32,
    socket: UnixStream,
    bell: Bell,
}

/// One slot of the outside bell set.
#[derive(Debug)]
struct BellSlot {
    generation: u32,
    bell: Option<Bell>,
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
/// make no system call beyond one eventfd write per doorbell that is asleep.
#[derive(Debug)]
pub struct Producer {
    ring: Ring,
    writer: BufferWriter<WriteMap>,
    frames: FramePool<()>,
    holds: HoldTable<ReadMap>,
    links: Vec<Link>,
    bells: Vec<BellSlot>,
    /// For each buffer, the lease on the frame the ring still holds in it. Kept until the ring
    /// takes the buffer back, so the buffer stays this producer's while any reader may read it.
    in_flight: Vec<Option<FrameLease<()>>>,
    geometry: Geometry,
    published: u64,
    dropped: u64,
    gone: u64,
}

impl Producer {
    /// A producer with `config`'s ring and buffers, all free and no readers yet. The buffers
    /// all sit in one memfd, at [`Geometry::offset`] each. The outside bell set is allocated
    /// here, [`OUTSIDE_BELLS`] long.
    ///
    /// # Errors
    ///
    /// [`SocketError`] if the shape is not allowed or the memory can't be made.
    pub fn new(config: ProducerConfig) -> Result<Self, SocketError> {
        let geometry = Geometry::new(config.payload, config.buffers, config.history)?;
        let ring = Ring::create(&geometry.ring_spec())?;
        let pool_fd = shm_ring::sealed_memfd(geometry.pool_bytes())?;
        let mut memories: Vec<Box<dyn BufferMemory>> = Vec::with_capacity(geometry.buffers());
        for index in 0..geometry.buffers() {
            memories.push(Box::new(FdMemory::memfd_at(
                pool_fd.try_clone()?,
                geometry.offset(index),
                geometry.buffer_bytes(),
                true,
            )?));
        }
        let frames: FramePool<()> = FramePool::new(
            BufferSize::new(geometry.payload(), geometry.slack()),
            memories,
        )?;
        let pool = Pool::new(1, vec![pool_fd], geometry.buffer_descs())?;
        let writer = BufferWriter::new(ring.writer()?, FIELD, pool);
        Ok(Self {
            ring,
            writer,
            frames,
            holds: HoldTable::new(geometry.buffers() as u32),
            links: Vec::new(),
            bells: (0..OUTSIDE_BELLS)
                .map(|_| BellSlot {
                    generation: 0,
                    bell: None,
                })
                .collect(),
            in_flight: (0..geometry.buffers()).map(|_| None).collect(),
            geometry,
            published: 0,
            dropped: 0,
            gone: 0,
        })
    }

    /// Hands a connected reader the ring and the pool, then registers the marks and the
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
        let pool_fd = self
            .writer
            .pool()
            .descriptors()
            .next()
            .ok_or(SocketError::Protocol("the pool has no memfd"))?;

        let offer = encode_offer(&Offer {
            slot,
            max_held: limits.max_held,
            geometry: self.geometry,
        });
        let fds = [ring_fd.as_fd(), page_fd.as_fd(), pool_fd];
        fdpass::send_on(&socket, &[&offer], &fds)?;

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

    /// Adds a doorbell that does not belong to one of this producer's socket readers: an
    /// Orion subscriber's, say. It is rung with the readers' on every [`Producer::publish`],
    /// until [`Producer::remove_bell`]. `eventfd` and `page` are the doorbell's two descriptors
    /// ([`shm_ring_doorbell::Doorbell::bell_fds`]). [`Producer::remove_bell`] drops the bell and
    /// closes its descriptors.
    ///
    /// # Errors
    ///
    /// [`SocketError::NoBellSlot`] when all [`OUTSIDE_BELLS`] are in use;
    /// [`SocketError::Ring`] when the page can't be mapped. Nothing is added then.
    pub fn add_bell(&mut self, eventfd: OwnedFd, page: OwnedFd) -> Result<BellId, SocketError> {
        let slot = self
            .bells
            .iter()
            .position(|slot| slot.bell.is_none())
            .ok_or(SocketError::NoBellSlot)?;
        let bell = Bell::attach(eventfd, page)?;
        let entry = &mut self.bells[slot];
        entry.generation = entry.generation.wrapping_add(1);
        entry.bell = Some(bell);
        Ok(BellId {
            slot: slot as u32,
            generation: entry.generation,
        })
    }

    /// Removes an outside doorbell added by [`Producer::add_bell`]: it is not rung again, and
    /// its descriptors are closed. An id that is no longer current does nothing.
    pub fn remove_bell(&mut self, bell: BellId) {
        let Some(entry) = self.bells.get_mut(bell.slot as usize) else {
            return;
        };
        if entry.generation == bell.generation {
            entry.bell = None;
        }
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
    /// ring entry and rings each sleeping doorbell once: the readers' and the outside ones. The
    /// lease is kept until the ring takes the buffer back.
    ///
    /// # Errors
    ///
    /// [`SocketError::Buffer`] if the ring refuses the buffer. Nothing is published.
    pub fn publish(&mut self, lease: FrameLease<()>) -> Result<u64, SocketError> {
        let index = lease.buffer_index();
        let entry = *lease.entry();
        let mut meta = [0_u8; META_MAX];
        let len = encode_meta(&entry, &mut meta);
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
        for slot in &mut self.bells {
            if let Some(bell) = slot.bell.as_mut() {
                bell.ring();
            }
        }
        Ok(seq)
    }

    /// The shape the producer runs with: what [`Producer::new`] was asked for, resolved.
    pub const fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// The payload size of each buffer.
    pub const fn payload(&self) -> usize {
        self.geometry.payload()
    }

    /// The ring this producer writes. Borrowed: an adopter (Orion's topic, say) takes its own
    /// descriptors of it and reads and writes the same ring.
    pub const fn ring(&self) -> &Ring {
        &self.ring
    }

    /// The buffer pool this producer fills. Borrowed, like [`Producer::ring`].
    pub fn pool(&self) -> &Pool {
        self.writer.pool()
    }

    /// The hold table: which reader holds which buffer, and the limits. Borrowed, like
    /// [`Producer::ring`].
    pub const fn holds(&self) -> &HoldTable<ReadMap> {
        &self.holds
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
