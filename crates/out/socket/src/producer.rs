//! The producer: the camera service's end of the shared ring.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use shm_fdpass as fdpass;
use shm_ring::{FieldId, ReadMap, Ring, SampleTime, WriteMap};
use shm_ring_buffers::{
    BufferDesc, BufferIndex, BufferWriter, Dropped, Holders, MAX_READERS, Pool, page,
};
use shm_ring_doorbell::Bell;
use shm_ring_holds::{HoldTable, Limits};
use styx_frame::{
    AcquireError, BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool, MemoryKind,
};

use crate::geometry::Geometry;
use crate::state::{Carried, ProducerState, StateWriter};
use crate::wire::{
    ACK_LEN, MAX_DESCRIPTORS, META_MAX, Offer, Record, VERSION, encode_entry, encode_offer,
    encode_record, version_of,
};
use crate::{SocketError, StreamEnd};

/// How long a reader has to answer the handshake.
/// How long a reader has to answer a handshake, from its connection, before it is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A reader's handshake, half done: the slot it will hold, and the owned descriptors and records
/// the handshake sends. See [`Producer::handshake_plan`].
#[derive(Debug)]
pub struct HandshakePlan {
    slot: u32,
    geometry: Geometry,
    ring_fd: OwnedFd,
    page_fd: OwnedFd,
    state_fd: OwnedFd,
    descriptors: Vec<OwnedFd>,
    buffers: Vec<Record>,
}

/// A reader that answered its handshake: its socket and the reader's marks and doorbell, ready for
/// [`Producer::attach`].
#[derive(Debug)]
pub struct Accepted {
    slot: u32,
    socket: UnixStream,
    marks: OwnedFd,
    limits: Limits,
    eventfd: OwnedFd,
    bell_page: OwnedFd,
}

impl HandshakePlan {
    /// The reader slot this handshake offers.
    pub const fn slot(&self) -> u32 {
        self.slot
    }

    /// Runs the handshake on `socket`: the offer, the records, then the reader's answer. Blocks
    /// for the reader, at most `within` for each message. Does not touch the producer, so it may
    /// run on any thread. A reader that does not answer in time gets an error, and its socket is
    /// dropped with the plan's result.
    ///
    /// # Errors
    ///
    /// [`SocketError`] when the reader does not answer as asked, or does not answer in time.
    pub fn run(
        &self,
        socket: UnixStream,
        limits: Limits,
        within: Duration,
    ) -> Result<Accepted, SocketError> {
        let slot = self.slot;
        socket.set_read_timeout(Some(within))?;
        socket.set_write_timeout(Some(within))?;
        let offer = encode_offer(&Offer {
            slot,
            max_held: limits.max_held,
            geometry: self.geometry,
            descriptors: self.descriptors.len() as u32,
        });
        let fds = [
            self.ring_fd.as_fd(),
            self.page_fd.as_fd(),
            self.state_fd.as_fd(),
        ];
        fdpass::send_on(&socket, &[&offer], &fds)?;

        let mut named = 0_usize;
        for record in &self.buffers {
            let bytes = encode_record(record).ok_or(SocketError::Protocol(
                "a buffer's kind does not cross processes",
            ))?;
            if record.descriptor as usize == named {
                let fd = self.descriptors.get(named).ok_or(SocketError::Protocol(
                    "a buffer names a descriptor that is not there",
                ))?;
                fdpass::send_on(&socket, &[&bytes], &[fd.as_fd()])?;
                named += 1;
            } else {
                fdpass::send_on(&socket, &[&bytes], &[])?;
            }
        }
        if named != self.descriptors.len() {
            return Err(SocketError::Protocol("a descriptor no buffer names"));
        }

        let mut ack = [0_u8; ACK_LEN];
        let received = fdpass::recv(socket.as_fd(), &mut ack, 3, false)?.ok_or(
            SocketError::Protocol("the reader closed before it answered"),
        )?;
        // The version first: an old reader's answer may not be of this length.
        if let Some(theirs) = version_of(ack.get(..received.len).unwrap_or_default())
            && theirs != VERSION
        {
            return Err(SocketError::Version {
                ours: VERSION,
                theirs,
            });
        }
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
        socket.set_read_timeout(None)?;
        socket.set_write_timeout(None)?;
        Ok(Accepted {
            slot,
            socket,
            marks,
            limits,
            eventfd,
            bell_page,
        })
    }
}
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
    /// This reader's marks, mapped a second time, in a table of its own. The hold table answers for
    /// all readers together; this one answers which buffers this reader marks, which the reserve
    /// needs to cut off the right reader. `None` once the reader is cut off for the reserve.
    marks: Option<HoldTable<ReadMap>>,
    /// Cut off for the reserve (see [`Producer::reclaim`]). Its slot is then out of the hold table,
    /// and it stays linked until its socket closes, so the slot is not given to another reader.
    reserve_cut: bool,
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
    /// Readers cut off, for their limits or for the reserve (see [`Producer::reclaim`]).
    pub cut_off: usize,
    /// Readers cut off for the reserve since the producer started: a running total. A reader cut
    /// off this way is counted in `cut_off` until its socket closes; this total keeps it.
    pub cut_off_for_reserve: u64,
    /// Readers whose socket closed (process exit or drop); their holds were removed.
    pub gone: u64,
    /// The configured keep window: the most of the newest published frames kept (see
    /// [`Producer::set_keep`]).
    pub keep: usize,
    /// The keep window in force now: smaller than `keep` while readers hold older frames and the
    /// reserve needs the buffers (see [`Producer::reclaim`]).
    pub kept: usize,
    /// Buffers the camera keeps for itself (see [`Producer::set_reserve`]).
    pub reserve: usize,
    /// Buffers out of the camera's hands now: the frames the ring still holds, in the keep window
    /// or held by readers past it. The invariant is `outstanding <= buffers - reserve`.
    pub outstanding: usize,
    /// Publishes after which the reserve could not be kept: a frame past the window was held by a
    /// mark the producer cannot cut off (one an adopter put in the hold table). Zero otherwise.
    pub bound_unmet: u64,
    /// Frames that left the keep window and were given back to the pool by [`Producer::reclaim`].
    /// Whether a reader read each one first is not visible here: the reader counts its own skips.
    pub reclaimed: u64,
}

/// One connected reader, for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderInfo {
    /// The reader's slot in the ring.
    pub slot: u32,
    /// Whether the reader has been cut off, for its limits or for the reserve.
    pub cut_off: bool,
    /// Whether the reader was cut off for the reserve (it held a frame past the keep window while
    /// the camera needed its buffer).
    pub for_reserve: bool,
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
    /// How many of the newest published frames [`Producer::reclaim`] keeps at most, held by a
    /// reader or not. A reader that is within `keep` frames of the newest finds its frame. See
    /// [`Producer::set_keep`].
    keep: usize,
    /// The window in force at the last [`Producer::reclaim`]: `keep`, or less while readers hold
    /// older frames and the reserve needs the buffers. See [`Producer::kept_now`].
    kept: usize,
    /// Buffers the camera keeps for itself to stream: the buffers out of the ring's hands never
    /// exceed `buffers - reserve`. See [`Producer::set_reserve`] and [`Producer::reclaim`].
    reserve: usize,
    /// Which reader slots have a reader linked: a slot stays taken while its reader is linked,
    /// cut off or not, so a cut-off reader's slot is not handed to another reader.
    taken: [bool; MAX_READERS as usize],
    /// Readers cut off for the reserve, in total.
    cut_for_reserve: u64,
    /// Publishes after which the reserve could not be kept (see [`ProducerStats::bound_unmet`]).
    bound_unmet: u64,
    /// For each buffer, the publish count (1-based) of the frame it holds: the frame's age is
    /// `published - seq_of[buffer]`.
    seq_of: Vec<u64>,
    /// The memory kind of each buffer, as the handshake names it.
    kinds: Vec<MemoryKind>,
    /// The producer's state page: running or retrying, shared read-only with every reader.
    state: StateWriter,
    geometry: Geometry,
    published: u64,
    dropped: u64,
    gone: u64,
    reclaimed: u64,
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
            keep: 1,
            kept: 1,
            reserve: 0,
            taken: [false; MAX_READERS as usize],
            cut_for_reserve: 0,
            bound_unmet: 0,
            seq_of: vec![0; geometry.buffers()],
            kinds,
            state: StateWriter::create()?,
            geometry,
            published: 0,
            dropped: 0,
            gone: 0,
            reclaimed: 0,
        })
    }

    /// Sets the configured keep window: the most of the newest published frames that
    /// [`Producer::reclaim`] keeps whether or not a reader holds them, so a reader that is up to
    /// `keep - 1` frame periods late still finds every frame. Frames older than the window are
    /// given back unless a reader holds them. The window shrinks, down to the newest frame, while
    /// readers hold older frames and the reserve needs the buffers (see [`Producer::kept_now`]),
    /// and grows back to `keep` as those holds are released. The camera service sets it to
    /// `min(history, buffers - hardware_reserve)`.
    ///
    /// `keep` is clamped to `1..=min(history, buffers - reserve)`, so that `keep + reserve` never
    /// exceeds the pool; the value in force is returned. The default is 1 (the newest frame only,
    /// the ring's own policy).
    pub fn set_keep(&mut self, keep: usize) -> usize {
        let limit = (self.geometry.history() as usize)
            .min(self.geometry.buffers().saturating_sub(self.reserve));
        self.keep = keep.clamp(1, limit.max(1));
        self.kept = self.keep;
        self.keep
    }

    /// The keep window in force now (at most [`Producer::keep`]). It shrinks, to 1 at the least,
    /// while readers hold older frames and the reserve needs their buffers, and grows back to
    /// `keep` as those holds are released.
    pub const fn kept_now(&self) -> usize {
        self.kept
    }

    /// The configured keep window, the maximum (see [`Producer::set_keep`]).
    pub const fn keep(&self) -> usize {
        self.keep
    }

    /// Sets the reserve: how many buffers the camera keeps for itself to stream. The invariant is
    /// that the buffers out of the camera's hands (the frames in the keep window, plus the frames
    /// readers hold past it) never exceed `buffers - reserve`, so the camera always has at least
    /// `reserve` buffers free or in its own flight. [`Producer::reclaim`] enforces it by cutting off
    /// the reader that holds the oldest frame past the window, when the bound needs it.
    ///
    /// `reserve` is clamped to `buffers - keep`, so `keep + reserve <= buffers`; the value in
    /// force is returned. The default is 0: the bound is then the whole pool, and nothing is cut
    /// off for it.
    pub fn set_reserve(&mut self, reserve: usize) -> usize {
        self.reserve = reserve.min(self.geometry.buffers().saturating_sub(self.keep));
        self.reserve
    }

    /// The reserve in force (see [`Producer::set_reserve`]).
    pub const fn reserve(&self) -> usize {
        self.reserve
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
        let plan = self.handshake_plan()?;
        let accepted = plan.run(socket, limits, HANDSHAKE_TIMEOUT)?;
        self.attach(accepted)
    }

    /// The first half of a handshake, made on the producer's thread: reserves a reader slot and
    /// copies out, as owned descriptors, what the reader is sent. The plan owns no reference into
    /// the producer, so [`HandshakePlan::run`] can be run on another thread while frames keep
    /// being published. A plan is for one reader: give it to [`HandshakePlan::run`], then the
    /// result to [`Producer::attach`]. No other reader is attached in between.
    ///
    /// # Errors
    ///
    /// [`SocketError::NoReaderSlot`] when every slot is taken; [`SocketError`] when a descriptor
    /// cannot be duplicated.
    pub fn handshake_plan(&self) -> Result<HandshakePlan, SocketError> {
        let slot = (0..MAX_READERS)
            .find(|slot| !self.taken[*slot as usize])
            .ok_or(SocketError::NoReaderSlot)?;
        let ring_fd = self.ring.read_fd()?.try_clone()?;
        let page_fd = self.writer.pool().page_fd()?.try_clone()?;
        let state_fd = self.state.reader_fd()?;
        let descriptors: Vec<OwnedFd> = self
            .writer
            .pool()
            .descriptors()
            .map(|fd| fd.try_clone_to_owned())
            .collect::<io::Result<_>>()?;
        let mut buffers = Vec::with_capacity(self.kinds.len());
        for (index, (desc, kind)) in self
            .writer
            .pool()
            .buffers()
            .iter()
            .zip(&self.kinds)
            .enumerate()
        {
            buffers.push(Record {
                index: index as u32,
                descriptor: u32::from(desc.descriptor),
                kind: *kind,
                offset: desc.offset,
                len: desc.size,
            });
        }
        Ok(HandshakePlan {
            slot,
            geometry: self.geometry,
            ring_fd,
            page_fd,
            state_fd,
            descriptors,
            buffers,
        })
    }

    /// The second half of a handshake: the reader's marks and doorbell go into the hold table,
    /// and the reader is linked, so the next frame can be published to it. The reader's doorbell
    /// is rung once, for any frame published since it took the ring. Quick and non-blocking;
    /// call it on the producer's thread between frames.
    ///
    /// # Errors
    ///
    /// [`SocketError`] if the reader's marks or doorbell cannot be mapped. The reader is then
    /// dropped (its socket closes).
    pub fn attach(&mut self, accepted: Accepted) -> Result<u32, SocketError> {
        let Accepted {
            slot,
            socket,
            marks,
            limits,
            eventfd,
            bell_page,
        } = accepted;
        // The same marks, mapped again for this reader alone (see `Link::marks`). Read-only, as
        // the table's own mapping is.
        let mut own: HoldTable<ReadMap> = HoldTable::new(self.geometry.buffers() as u32);
        own.add(0, marks.try_clone()?, Limits::default())?;
        self.holds.add(slot, marks, limits)?;
        let mut bell = Bell::attach(eventfd, bell_page)?;
        // The reader has had the ring since the first half of the handshake and may already be
        // asleep on its doorbell. A frame published between then and now rang nothing for it, so
        // it would sit unread until the next publish: one frame period late. One ring here wakes
        // it for whatever is already there; with nothing there it looks and sleeps again.
        bell.ring();
        self.taken[slot as usize] = true;
        self.links.push(Link {
            slot,
            socket,
            bell,
            marks: Some(own),
            reserve_cut: false,
        });
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
    /// When every buffer is held or in the keep window, the ring takes back its oldest frame
    /// even inside the window (the window is for readers, and a frame is never refused for it).
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
        self.published += 1;
        self.seq_of[index] = self.published;
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

    /// Ends the ring cleanly ([`Producer::end`] with [`StreamEnd::Closed`]). Dropping the producer
    /// does this too.
    pub fn close(&mut self) {
        self.end(StreamEnd::Closed);
    }

    /// Ends the stream with `end`: readers get the frames already published, then
    /// [`ReadError::Ended`] with this end from their next read, and a reader waiting on its
    /// doorbell wakes at once. Idempotent: a ring already ended keeps its first end. Writing
    /// [`StreamEnd::ConsumerFault`] is for a test or a reader-side report only; the camera
    /// service writes `Closed` or `DeviceFailed`. [`StreamEnd::Unknown`] with 0 is not an end,
    /// so it is written as `Closed`.
    pub fn end(&mut self, end: StreamEnd) {
        if self.ring.end() == 0 {
            let code = match end.code() {
                0 => crate::ENDED_CLOSED,
                code => code,
            };
            self.ring.set_end(code);
        }
        self.ring_all();
    }

    /// The camera failed transiently: the stream stays attached and its readers stay linked, and
    /// they are told `Retrying` since `since_ns` (CLOCK_MONOTONIC nanoseconds). Every reader's
    /// doorbell and every outside bell is rung once, so a reader asleep wakes with no frame.
    /// Idempotent: a stall already in force keeps its first time. See [`ProducerState`].
    pub fn stall(&mut self, since_ns: u64) {
        if matches!(self.state.state(), ProducerState::Retrying { .. }) {
            return;
        }
        if self
            .state
            .change(ProducerState::Retrying { since_ns }, self.state.recovery())
        {
            self.ring_all();
        }
    }

    /// The camera came back. `carried` says whether the same driver instance recovered
    /// ([`Carried::Kept`]) or the device was rebound ([`Carried::Lost`]). A reader that was told
    /// of the stall gets [`crate::Next::Resumed`] before the next frame. Does nothing when the
    /// camera already runs.
    pub fn resume(&mut self, carried: Carried) {
        if self.state.change(ProducerState::Running, carried) {
            self.ring_all();
        }
    }

    /// The producer's state now: running, or retrying since when.
    pub fn producer_state(&self) -> ProducerState {
        self.state.state()
    }

    /// Rings every linked reader's doorbell and every outside bell, once each.
    fn ring_all(&mut self) {
        for link in &mut self.links {
            link.bell.ring();
        }
        for slot in &mut self.bells {
            if let Some(bell) = slot.bell.as_mut() {
                bell.ring();
            }
        }
    }

    /// Gives back the buffers of frames that have left the keep window and that no reader holds.
    /// The newest `keep` frames are kept whether or not a reader holds them; a frame older than
    /// that that a reader holds is given back when the reader lets go (it is retried here). A
    /// given-back frame's lease is dropped, so its pool buffer is free for a stream or a device.
    /// Returns how many were given back. Call it after publishing.
    ///
    /// **The reserve.** At every call, the buffers out of the camera's hands (the frames the ring
    /// still holds: those in the keep window and those held by readers past it) are at most
    /// `buffers - reserve`. A frame a reader holds takes precedence over the window: first the
    /// window shrinks, giving back the frames inside it that no reader holds, oldest first, down
    /// to the newest frame alone. Only if the bound is still not met is a reader cut off: the one
    /// holding the oldest frame past the window, whose marks leave the hold table. It is told
    /// through the pool page and woken, and reads [`crate::ReadError::Ended`] with
    /// [`StreamEnd::ConsumerFault`]. So a reader may hold up to `buffers - reserve - 1` frames
    /// older than the newest, and holding more cuts it off.
    ///
    /// Known residuals (limits of the ring crate at this revision):
    /// - a mark an adopter writes directly into the hold table ([`Producer::holds_mut`]) is
    ///   invisible to the producer and cannot be cut off here; then the bound is unmet and
    ///   [`ProducerStats::bound_unmet`] counts the publishes on which that happened;
    /// - a cut reader may still be reading the frame it held when its buffer is reused. It must
    ///   drop every frame it holds on [`crate::ReadError::Ended`], and the content of its last frame
    ///   is not guaranteed.
    ///
    /// No frame is copied and nothing waits.
    pub fn reclaim(&mut self) -> usize {
        let budget = self.geometry.buffers() - self.reserve;
        // The window in force for this publish starts at the configured keep and only shrinks.
        let mut window = self.keep;
        let mut given = self.release_unheld_from(window);
        // First the window yields: frames inside it that no reader holds are given back, oldest
        // first, down to the newest frame alone.
        while self.outstanding() > budget && window > 1 {
            window -= 1;
            given += self.release_unheld_from(window);
        }
        // Only if the bound still is not met (the readers' held frames alone exceed it beside the
        // newest frame) is a reader cut off: the one holding the oldest frame past the window.
        while self.outstanding() > budget {
            let Some(index) = self.oldest_from(window) else {
                break;
            };
            self.cut_off_holders(index);
            given += self.release_unheld_from(window);
            if self.in_flight[index].is_some() {
                // A mark the producer cannot see (an adopter's): the bound stays unmet.
                self.bound_unmet += 1;
                break;
            }
        }
        self.kept = window;
        given
    }

    /// Gives back every frame at or past `window` ages that no reader holds. Returns how many.
    fn release_unheld_from(&mut self, window: usize) -> usize {
        let mut given = 0;
        for index in 0..self.in_flight.len() {
            if self.in_flight[index].is_none() || self.age_of(index) < window as u64 {
                continue;
            }
            if self.writer.reclaim(BufferIndex(index as u32), &self.holds) {
                self.release(index);
                given += 1;
            }
        }
        given
    }

    /// Frames the producer's ring has published that the camera has not got back: its own
    /// `in_flight` view. Buffers in the camera's hands are `buffers - outstanding()`.
    fn outstanding(&self) -> usize {
        self.in_flight
            .iter()
            .filter(|lease| lease.is_some())
            .count()
    }

    /// How many publishes ago buffer `index`'s frame was published (0 for the newest).
    fn age_of(&self, index: usize) -> u64 {
        self.published - self.seq_of[index]
    }

    /// The oldest frame the ring still holds that is at least `window` publishes old, if any.
    fn oldest_from(&self, window: usize) -> Option<usize> {
        let window = window as u64;
        let mut oldest: Option<(u64, usize)> = None;
        for index in 0..self.in_flight.len() {
            if self.in_flight[index].is_none() || self.age_of(index) < window {
                continue;
            }
            if oldest.is_none_or(|(seq, _)| self.seq_of[index] < seq) {
                oldest = Some((self.seq_of[index], index));
            }
        }
        oldest.map(|(_, index)| index)
    }

    /// Gives back buffer `index`'s frame to the camera's side: its lease is dropped.
    fn release(&mut self, index: usize) {
        self.in_flight[index] = None;
        self.reclaimed += 1;
    }

    /// Cuts off every reader that marks buffer `index`, for the reserve. Each one's marks leave the
    /// hold table, so its holds stop counting at once, and the table's own mapping of its slot is
    /// cleared with them; the reader is told through the pool page and its doorbell is rung, so a
    /// reader asleep wakes and reads the end. The link stays, with the slot taken, until the socket
    /// closes (then [`Producer::reap`] removes it as it does any other). Readers the hold table has
    /// already cut off by their own limits do not hold anything and are skipped.
    fn cut_off_holders(&mut self, index: usize) {
        let buffer = BufferIndex(index as u32);
        let buffers = self.geometry.buffers() as u32;
        for link in &mut self.links {
            if link.reserve_cut || self.holds.is_cut_off(link.slot) {
                continue;
            }
            let Some(own) = link.marks.as_ref() else {
                continue;
            };
            if !own.held(buffer) {
                continue;
            }
            self.holds.remove(link.slot, self.writer.pool().page());
            page::cut_off(self.writer.pool().page(), buffers, link.slot);
            link.bell.ring();
            link.marks = None;
            link.reserve_cut = true;
            self.cut_for_reserve += 1;
        }
    }

    /// Whether `link` has been cut off, for its limits or for the reserve.
    fn is_cut(&self, link: &Link) -> bool {
        link.reserve_cut || self.holds.is_cut_off(link.slot)
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
            keep: self.keep,
            kept: self.kept,
            reserve: self.reserve,
            outstanding: self.outstanding(),
            bound_unmet: self.bound_unmet,
            reclaimed: self.reclaimed,
            cut_off: self.links.iter().filter(|link| self.is_cut(link)).count(),
            cut_off_for_reserve: self.cut_for_reserve,
            gone: self.gone,
        }
    }

    /// Each connected reader's slot and whether it has been cut off, and for which reason.
    pub fn readers(&self) -> Vec<ReaderInfo> {
        self.links
            .iter()
            .map(|link| ReaderInfo {
                slot: link.slot,
                cut_off: self.is_cut(link),
                for_reserve: link.reserve_cut,
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
                self.taken[link.slot as usize] = false;
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

impl<M> Drop for Producer<M> {
    /// A producer that goes away has ended its ring: its readers see the end, not a silence.
    fn drop(&mut self) {
        self.close();
    }
}
