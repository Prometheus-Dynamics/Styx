//! The producer: the camera service's end of the shared ring.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use shm_fdpass as fdpass;
use shm_ring::{FieldId, ReadMap, Ring, SampleTime, WriteMap};
use shm_ring_buffers::{BufferDesc, BufferIndex, BufferWriter, Dropped, Pool};
use shm_ring_doorbell::Bell;
use shm_ring_holds::{HoldTable, Limits};
use styx_frame::{
    AcquireError, BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool, MemoryKind,
};

use crate::SocketError;
use crate::geometry::Geometry;
use crate::wire::{
    ACK_LEN, MAX_DESCRIPTORS, META_MAX, Offer, Record, VERSION, encode_entry, encode_offer,
    encode_record,
};

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
/// `M` is the pool's metadata type (`()` for [`Producer::new`]). A producer over a camera's pool
/// publishes that pool's leases as they are: [`Producer::publish`] takes a lease from the pool
/// and writes its buffer's index into the ring, with no copy. The buffer must be free in the ring
/// (see [`Producer::reclaim`]), and the lease is kept until the ring gives the buffer back.
///
/// Hold it on one thread: [`Producer::begin`] and [`Producer::publish`] are the frame path and
/// make no system call beyond one eventfd write per doorbell that is asleep.
#[derive(Debug)]
pub struct Producer<M = ()> {
    ring: Ring,
    writer: BufferWriter<WriteMap>,
    frames: FramePool<M>,
    holds: HoldTable<ReadMap>,
    links: Vec<Link>,
    bells: Vec<BellSlot>,
    /// For each buffer, the lease on the frame the ring still holds in it. Kept until the ring
    /// takes the buffer back, so the buffer stays this producer's while any reader may read it.
    in_flight: Vec<Option<FrameLease<M>>>,
    /// The buffer of the newest published frame. [`Producer::reclaim`] leaves it alone, so there
    /// is always a frame for a reader to find (as the ring's own `take` does).
    newest: Option<usize>,
    /// The memory kind of each buffer, as the handshake names it.
    kinds: Vec<MemoryKind>,
    geometry: Geometry,
    published: u64,
    dropped: u64,
    gone: u64,
}

impl Producer<()> {
    /// A producer with `config`'s ring and buffers, all free and no readers yet. The buffers
    /// all sit in one memfd, at [`Geometry::offset`] each, and the memfd is sealed against
    /// shrinking. The outside bell set is allocated here, [`OUTSIDE_BELLS`] long.
    ///
    /// # Errors
    ///
    /// [`SocketError`] if the shape is not allowed or the memory can't be made.
    pub fn new(config: ProducerConfig) -> Result<Self, SocketError> {
        let geometry = Geometry::new(config.payload, config.buffers, config.history)?;
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
        Self::with_pool(frames, config.history)
    }
}

impl<M> Producer<M> {
    /// A producer that publishes an existing pool's frames. Every buffer of `pool` must be
    /// registered and fd-backed: a memfd (sealed against shrinking, as `shm_ring::sealed_memfd`
    /// makes it) or a dma-buf, one descriptor per buffer or several buffers at offsets in one.
    /// `history` is the ring's entries, as [`ProducerConfig::history`].
    ///
    /// The camera takes its frames from the pool through [`Producer::begin`], which picks the
    /// buffer the ring can give, and fills the lease (or hands the buffer's descriptor to a
    /// device, by [`FrameLease::buffer_index`]). [`Producer::publish`] keeps the lease until the
    /// ring takes the buffer back, so a reader's hold keeps the frame in its buffer.
    ///
    /// The buffers the pool holds when it is given are the ones published; a buffer registered
    /// later is not. Readers get the pool's buffers, descriptors and kinds once, in the handshake.
    ///
    /// # Errors
    ///
    /// [`SocketError::Config`] when a buffer is not registered, has no descriptor, is host or
    /// GPU memory, or the pool spans more than [`MAX_DESCRIPTORS`] descriptors.
    /// [`SocketError::Ring`] or [`SocketError::Io`] when the ring or the descriptor identity
    /// can't be made.
    pub fn with_pool(pool: FramePool<M>, history: u32) -> Result<Self, SocketError> {
        let size = pool.buffer_size();
        let buffers = pool.buffers();
        let geometry = Geometry::checked(history, META_MAX, buffers, size.payload, size.gpu_slack)?;
        let mut fds: Vec<OwnedFd> = Vec::new();
        let mut identities: Vec<(u64, u64)> = Vec::new();
        let mut descs = Vec::with_capacity(buffers);
        let mut kinds = Vec::with_capacity(buffers);
        for index in 0..buffers {
            let memory = pool.memory(index).ok_or(SocketError::Config(
                "a buffer of the pool is not registered",
            ))?;
            let kind = memory.kind();
            if !matches!(kind, MemoryKind::Memfd | MemoryKind::Dmabuf) {
                return Err(SocketError::Config(
                    "only memfd and dma-buf buffers cross to other processes",
                ));
            }
            let fd = memory.fd().ok_or(SocketError::Config(
                "a buffer of the pool has no descriptor",
            ))?;
            let identity = file_identity(fd)?;
            let descriptor = match identities.iter().position(|known| *known == identity) {
                Some(known) => known,
                None => {
                    if fds.len() == MAX_DESCRIPTORS {
                        return Err(SocketError::Config(
                            "a pool's buffers sit in at most 256 descriptors",
                        ));
                    }
                    fds.push(fd.try_clone_to_owned()?);
                    identities.push(identity);
                    fds.len() - 1
                }
            };
            descs.push(BufferDesc {
                descriptor: descriptor as u8,
                offset: memory.fd_offset() as u64,
                size: memory.len() as u64,
            });
            kinds.push(kind);
        }
        let ring = Ring::create(&geometry.ring_spec())?;
        let shm_pool = Pool::new(1, fds, descs)?;
        let writer = BufferWriter::new(ring.writer()?, FIELD, shm_pool);
        Ok(Self {
            ring,
            writer,
            frames: pool,
            holds: HoldTable::new(geometry.buffers() as u32),
            links: Vec::new(),
            bells: (0..OUTSIDE_BELLS)
                .map(|_| BellSlot {
                    generation: 0,
                    bell: None,
                })
                .collect(),
            in_flight: (0..geometry.buffers()).map(|_| None).collect(),
            newest: None,
            kinds,
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
    /// The handshake is the offer (the ring's read end, the pool page, the geometry and the
    /// descriptor count), then one record per buffer: its descriptor, memory kind, offset and
    /// length. A record carries its descriptor only when it is the buffer that first names it,
    /// so a pool's buffers in one memfd cost one descriptor. Each message carries at most one
    /// descriptor; that keeps every message well under the socket's per-message limit.
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
        socket.set_write_timeout(Some(ANSWER_WITHIN))?;
        let ring_fd = self.ring.read_fd()?;
        let page_fd = self.writer.pool().page_fd()?;
        let descriptors: Vec<BorrowedFd<'_>> = self.writer.pool().descriptors().collect();

        let offer = encode_offer(&Offer {
            slot,
            max_held: limits.max_held,
            geometry: self.geometry,
            descriptors: descriptors.len() as u32,
        });
        let fds = [ring_fd.as_fd(), page_fd.as_fd()];
        fdpass::send_on(&socket, &[&offer], &fds)?;

        let mut named = 0_usize;
        for (index, (desc, kind)) in self
            .writer
            .pool()
            .buffers()
            .iter()
            .zip(&self.kinds)
            .enumerate()
        {
            let record = encode_record(&Record {
                index: index as u32,
                descriptor: u32::from(desc.descriptor),
                kind: *kind,
                offset: desc.offset,
                len: desc.size,
            })
            .ok_or(SocketError::Protocol(
                "a buffer's kind does not cross processes",
            ))?;
            if usize::from(desc.descriptor) == named {
                let fd = descriptors.get(named).ok_or(SocketError::Protocol(
                    "a buffer names a descriptor that is not there",
                ))?;
                fdpass::send_on(&socket, &[&record], &[*fd])?;
                named += 1;
            } else {
                fdpass::send_on(&socket, &[&record], &[])?;
            }
        }
        if named != descriptors.len() {
            return Err(SocketError::Protocol("a descriptor no buffer names"));
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
        socket.set_write_timeout(None)?;
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
    pub fn begin(&mut self, entry: FrameEntry) -> Result<FrameLease<M>, BeginError> {
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
    pub fn publish(&mut self, lease: FrameLease<M>) -> Result<u64, SocketError> {
        self.publish_with(lease, &[])
    }

    /// [`Producer::publish`], with `user` attached to the frame: opaque bytes the ring carries
    /// unread and the reader hands on with the frame ([`crate::Frame::user`]). At most
    /// [`Geometry::user_max`] bytes; a longer one is refused and nothing is published.
    ///
    /// A buffer the ring still has published (its frame not yet reclaimed) is refused with
    /// [`SocketError::Buffer`]; call [`Producer::reclaim`] to give back the buffers no reader holds.
    ///
    /// # Errors
    ///
    /// [`SocketError::Config`] for `user` too long; [`SocketError::Buffer`] when the ring refuses
    /// the buffer. In both cases the lease is dropped, so its buffer goes back to its pool.
    pub fn publish_with(&mut self, lease: FrameLease<M>, user: &[u8]) -> Result<u64, SocketError> {
        let index = lease.buffer_index();
        let entry = *lease.entry();
        let mut meta = [0_u8; META_MAX];
        let len = encode_entry(&entry, user, &mut meta)
            .ok_or(SocketError::Config("a frame's opaque metadata is too long"))?;
        // The frame's CPU write window ends before the ring names its buffer, so a dma-buf's sync
        // END is issued before any reader's START. A share taken and dropped at once ends the
        // window (`FrameLease::share` closes it) and keeps the lease's one hold.
        drop(lease.share());
        let seq = self.writer.publish(
            &[BufferIndex(index as u32)],
            SampleTime::from_nanos(entry.timestamp_ns),
            &meta[..len],
        )?;
        self.in_flight[index] = Some(lease);
        self.newest = Some(index);
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

    /// Gives back the buffers the ring no longer needs: each one no reader holds, except the newest
    /// frame's (which readers must still find). Its lease is dropped, so a pool buffer that is free
    /// after this can be given to a stream or a device again. Returns how many were given back.
    /// Call it after publishing, so the next frame's buffer is free when the pool offers it.
    pub fn reclaim(&mut self) -> usize {
        let mut given = 0;
        for index in 0..self.in_flight.len() {
            if self.newest == Some(index) || self.in_flight[index].is_none() {
                continue;
            }
            if self.writer.reclaim(BufferIndex(index as u32), &self.holds) {
                self.in_flight[index] = None;
                given += 1;
            }
        }
        given
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

    /// The same hold table, mutably: an adopter (Orion's subscriber, say) adds its marks here.
    pub const fn holds_mut(&mut self) -> &mut HoldTable<ReadMap> {
        &mut self.holds
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

/// The file a descriptor names, as device and inode: two buffers in one file have the same one,
/// whichever descriptor number each holds.
fn file_identity(fd: BorrowedFd<'_>) -> io::Result<(u64, u64)> {
    let file = std::fs::File::from(fd.try_clone_to_owned()?);
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

/// Whether the peer has closed its end. A reader never sends after its answer, so anything it
/// might send is left unread here; this only looks for end of stream.
fn peer_closed(socket: &UnixStream) -> bool {
    matches!(fdpass::recv(socket.as_fd(), &mut [], 0, true), Ok(None))
}
