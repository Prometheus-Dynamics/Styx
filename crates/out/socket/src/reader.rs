//! The reader: a process that takes the producer's frames in place.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use shm_fdpass as fdpass;
use shm_ring::{FieldId, ReadMap, Reader as RingReader, SampleTime};
use shm_ring_buffers::{Batch, BufferDesc, BufferReader, Held, NextBatch, PoolView};
use shm_ring_doorbell::{Doorbell, Wake};
use shm_ring_holds::Marks;
use styx_frame::{
    BufferMemory, BufferSize, CpuAccess, FdMemory, FrameEntry, FrameLease, FramePool, MemoryKind,
};

use crate::{SocketError, StreamEnd};
use crate::geometry::Geometry;
use crate::wire::{
    ACK_LEN, OFFER_LEN, Offer, RECORD_LEN, Record, USER_MAX, VERSION, decode_meta, decode_offer,
    decode_record, decode_user, version_of,
};

const FIELD: FieldId = FieldId(0);

/// Where a reader starts in the ring. Chosen at connect, by [`Reader::connect_with`]; the
/// default ([`Reader::connect`]) is [`Start::Newest`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Start {
    /// The newest frame the ring still holds, delivered at once if there is one, then every frame
    /// published after it. A reader that attaches mid-stream gets a frame without waiting for a
    /// bell. With nothing published yet it waits for the first frame, as `NextNew` does.
    #[default]
    Newest,
    /// Only the frames published after the reader attaches. Everything already in the ring is
    /// skipped (and not counted as lost: the reader did not ask for it).
    NextNew,
}

/// Why [`Reader::next_frame`] stopped delivering frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// The stream ended. Every frame already published has been delivered first; no more come.
    /// The [`StreamEnd`] says why. A cut-off reader gets [`StreamEnd::ConsumerFault`]: the
    /// producer cut it off for breaking its limits, and it takes no more frames. Drop the reader
    /// and connect again.
    Ended(StreamEnd),
    /// The producer replaced its pool. This reader does not have the new buffers.
    PoolChanged {
        /// The new pool's generation.
        generation: u32,
    },
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Ended(StreamEnd::Closed) => {
                f.write_str("the stream ended: the producer closed it, no more frames")
            }
            Self::Ended(StreamEnd::ConsumerFault) => f.write_str(
                "the stream ended because this reader broke the rules (a hold kept too long or a bad mark)",
            ),
            Self::Ended(StreamEnd::DeviceFailed) => {
                f.write_str("the stream ended because the device failed for good")
            }
            Self::Ended(StreamEnd::Unknown(code)) => {
                write!(f, "the stream ended, kind unknown (end code {code})")
            }
            Self::PoolChanged { generation } => {
                write!(
                    f,
                    "the producer replaced its pool (generation {generation})"
                )
            }
        }
    }
}

impl std::error::Error for ReadError {}

/// One frame, held. The buffer stays with the producer's ring until this is dropped; its bytes
/// are read where the producer wrote them.
#[derive(Debug)]
pub struct Frame {
    seq: u64,
    time: SampleTime,
    age_ns: u64,
    /// The frame as a `FrameLease` of this reader's pool. Declared before `held`, so the pool's
    /// slot is free before the ring's mark is taken off.
    lease: FrameLease<()>,
    held: Held<Arc<Marks>>,
    user: UserBytes,
}

/// The opaque bytes a producer attached to a frame, copied out of the ring entry (the ring's
/// slot is not the reader's to keep).
struct UserBytes {
    len: usize,
    bytes: [u8; USER_MAX],
}

impl core::fmt::Debug for UserBytes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserBytes").field("len", &self.len).finish()
    }
}

impl Frame {
    /// The opaque bytes the producer attached to this frame ([`crate::Producer::publish_with`]),
    /// empty when it attached none. This crate does not read them.
    pub fn user(&self) -> &[u8] {
        &self.user.bytes[..self.user.len]
    }

    /// The ring's sequence number of the frame's entry.
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// When the frame was captured, `CLOCK_MONOTONIC` nanoseconds.
    pub const fn time(&self) -> SampleTime {
        self.time
    }

    /// How old the frame was when this reader received it, in nanoseconds.
    pub const fn age_ns(&self) -> u64 {
        self.age_ns
    }

    /// The frame: read its planes with [`FrameLease::read`].
    pub const fn lease(&self) -> &FrameLease<()> {
        &self.lease
    }

    /// Which buffer of the pool holds the frame.
    pub fn buffer(&self) -> usize {
        self.lease.buffer_index()
    }

    /// Whether the frame's ring entry names exactly one buffer, as every frame this crate
    /// publishes does.
    pub fn is_held(&self) -> bool {
        self.held.len() == 1
    }
}

/// Counts for one reader, for reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderStats {
    /// Frames delivered to the caller.
    pub delivered: u64,
    /// Frames the ring had already overwritten, or whose buffer was taken back, before this
    /// reader got to them.
    pub skipped: u64,
    /// Frames [`Reader::latest`] passed over for a newer one.
    pub superseded: u64,
    /// Times this reader was at its own limit on held buffers and had to skip a frame.
    pub limited: u64,
    /// Entries this build could not read, or whose buffer the pool refused.
    pub malformed: u64,
    /// Buffers this reader holds now.
    pub held: u32,
    /// Entries published that this reader has not looked at yet: its queue depth.
    pub queued: u64,
    /// Dma-buf CPU sync brackets this reader has made (a start and an end each), for the
    /// buffers it maps as dma-bufs.
    pub dma_brackets: u64,
    /// Dma-buf sync calls the kernel refused. Zero on a working dma-buf path.
    pub dma_sync_errors: u64,
    /// Age of the last frame delivered, in nanoseconds.
    pub age_last_ns: u64,
    /// Oldest age delivered, in nanoseconds.
    pub age_max_ns: u64,
    /// Mean age over the frames delivered, in nanoseconds.
    pub age_mean_ns: u64,
}

#[derive(Debug, Default)]
struct Counters {
    delivered: u64,
    superseded: u64,
    limited: u64,
    malformed: u64,
    age_sum: u64,
    age_max: u64,
    age_last: u64,
}

impl Counters {
    fn record(&mut self, age_ns: u64) {
        self.delivered += 1;
        self.age_sum = self.age_sum.saturating_add(age_ns);
        self.age_max = self.age_max.max(age_ns);
        self.age_last = age_ns;
    }
}

/// The dma-buf sync brackets and refusals of one reader's buffers.
#[derive(Debug, Default)]
struct SyncTally {
    brackets: AtomicU64,
    errors: AtomicU64,
}

/// One buffer of a reader's pool: its descriptor mapped read-only, with the dma-buf sync
/// brackets it makes counted in the reader's tally. Everything else is delegated unchanged.
struct Bracketed {
    memory: FdMemory,
    tally: Arc<SyncTally>,
}

impl Bracketed {
    fn bracket(&self, call: impl FnOnce(&FdMemory)) {
        if self.memory.kind() != MemoryKind::Dmabuf {
            call(&self.memory);
            return;
        }
        let before = self.memory.sync_errors();
        call(&self.memory);
        self.tally.brackets.fetch_add(1, Ordering::Relaxed);
        let refused = self.memory.sync_errors().saturating_sub(before);
        if refused > 0 {
            self.tally
                .errors
                .fetch_add(refused as u64, Ordering::Relaxed);
        }
    }
}

// SAFETY: every method forwards to `memory`, an `FdMemory` that upholds `BufferMemory`'s contract
// for its mapping (a read-only shared mapping this value owns until it drops). The sync brackets
// only count and forward, so the bytes and pointer are exactly what `memory` gives.
unsafe impl BufferMemory for Bracketed {
    fn kind(&self) -> MemoryKind {
        self.memory.kind()
    }

    fn len(&self) -> usize {
        self.memory.len()
    }

    fn cpu_ptr(&self) -> Option<NonNull<u8>> {
        self.memory.cpu_ptr()
    }

    fn cpu_writable(&self) -> bool {
        self.memory.cpu_writable()
    }

    fn begin_cpu(&self, access: CpuAccess) {
        self.bracket(|memory| memory.begin_cpu(access));
    }

    fn end_cpu(&self, access: CpuAccess) {
        self.bracket(|memory| memory.end_cpu(access));
    }

    fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.memory.fd()
    }

    fn fd_offset(&self) -> usize {
        self.memory.fd_offset()
    }
}

/// A reader of one producer's frames.
#[derive(Debug)]
pub struct Reader {
    /// Kept open: the producer counts the reader as alive while this socket is.
    socket: UnixStream,
    buffers: BufferReader<ReadMap, Arc<Marks>>,
    marks: Arc<Marks>,
    frames: FramePool<()>,
    tally: Arc<SyncTally>,
    doorbell: Doorbell,
    geometry: Geometry,
    counters: Counters,
}

impl Reader {
    /// [`Reader::connect_with`] with [`Start::Newest`].
    ///
    /// # Errors
    ///
    /// As [`Reader::connect_with`].
    pub fn connect(socket: UnixStream) -> Result<Self, SocketError> {
        Self::connect_with(socket, Start::Newest)
    }

    /// Takes the producer's offer on `socket` and its buffer table, maps the buffers once, and
    /// answers with this reader's marks and doorbell. Blocks until the offer arrives.
    ///
    /// The reader's place in the ring is fixed when it answers, from the ring's write position
    /// at that moment, as `start` says (see [`Start`]). The producer links the reader right after
    /// it reads the answer, so the reader starts where the first frame it can be sent begins.
    ///
    /// # Errors
    ///
    /// [`SocketError`] if the offer or the buffer table is not the one this build understands,
    /// or a mapping fails.
    pub fn connect_with(socket: UnixStream, start: Start) -> Result<Self, SocketError> {
        let mut offer_bytes = [0_u8; OFFER_LEN];
        let received = fdpass::recv(socket.as_fd(), &mut offer_bytes, 2, false)?.ok_or(
            SocketError::Protocol("the producer closed before its offer"),
        )?;
        // The version first: another version may send an offer of another length.
        if let Some(theirs) = version_of(offer_bytes.get(..received.len).unwrap_or_default())
            && theirs != VERSION
        {
            return Err(SocketError::Version {
                ours: VERSION,
                theirs,
            });
        }
        if received.len != OFFER_LEN {
            return Err(SocketError::Protocol(
                "the offer is not this protocol version",
            ));
        }
        let Offer {
            slot,
            max_held,
            geometry,
            descriptors,
        } = decode_offer(&offer_bytes).ok_or(SocketError::Protocol(
            "the offer is not one this build reads",
        ))?;
        let mut fds = received.fds;
        if fds.len() != 2 {
            return Err(SocketError::Protocol(
                "the offer carries the wrong descriptors",
            ));
        }
        let (Some(ring_fd), Some(page_fd)) = (fds.take(0), fds.take(1)) else {
            return Err(SocketError::Protocol(
                "the offer lacks the ring or the pool page",
            ));
        };

        // The buffer table: one record per buffer, in order. A record carries its descriptor
        // exactly when it is the first buffer that names it.
        let descriptors = descriptors as usize;
        let mut pool_fds: Vec<OwnedFd> = Vec::with_capacity(descriptors);
        let mut records: Vec<Record> = Vec::with_capacity(geometry.buffers());
        for index in 0..geometry.buffers() {
            let mut bytes = [0_u8; RECORD_LEN];
            let received = fdpass::recv(socket.as_fd(), &mut bytes, 1, false)?.ok_or(
                SocketError::Protocol("the producer closed inside its buffer table"),
            )?;
            if received.len != RECORD_LEN {
                return Err(SocketError::Protocol("a buffer record is the wrong size"));
            }
            let carried = received.fds.len();
            let mut fds = received.fds;
            let record = decode_record(&bytes).ok_or(SocketError::Protocol(
                "a buffer record this build does not read",
            ))?;
            if record.index as usize != index {
                return Err(SocketError::Protocol("buffer records are out of order"));
            }
            let descriptor = record.descriptor as usize;
            if descriptor == pool_fds.len() && descriptor < descriptors {
                if carried != 1 {
                    return Err(SocketError::Protocol(
                        "a new descriptor came without its fd",
                    ));
                }
                pool_fds.push(fds.take(0).ok_or(SocketError::Protocol(
                    "a new descriptor came without its fd",
                ))?);
            } else if descriptor < pool_fds.len() {
                if carried != 0 {
                    return Err(SocketError::Protocol(
                        "a known descriptor came with an fd again",
                    ));
                }
            } else {
                return Err(SocketError::Protocol(
                    "a buffer names a descriptor the offer did not declare",
                ));
            }
            records.push(record);
        }
        if pool_fds.len() != descriptors {
            return Err(SocketError::Protocol(
                "the buffers name fewer descriptors than the offer",
            ));
        }

        // The ring's own view of the buffers: read-only, checked by the ring crate.
        let mut descs = Vec::with_capacity(records.len());
        for record in &records {
            descs.push(BufferDesc {
                descriptor: u8::try_from(record.descriptor)
                    .map_err(|_| SocketError::Protocol("a descriptor index is out of range"))?,
                offset: record.offset,
                size: record.len,
            });
        }
        // Our own FrameLease view of the same buffers, mapped read-only, one per buffer.
        let tally = Arc::new(SyncTally::default());
        let mut memories: Vec<Box<dyn BufferMemory>> = Vec::with_capacity(records.len());
        for record in &records {
            let fd = pool_fds[record.descriptor as usize].try_clone()?;
            let offset = usize::try_from(record.offset)
                .map_err(|_| SocketError::Config("a buffer's offset is too large"))?;
            let len = usize::try_from(record.len)
                .map_err(|_| SocketError::Config("a buffer is too large"))?;
            let memory = match record.kind {
                MemoryKind::Dmabuf => FdMemory::dmabuf_at(fd, offset, len, false)?,
                _ => FdMemory::memfd_at(fd, offset, len, false)?,
            };
            memories.push(Box::new(Bracketed {
                memory,
                tally: Arc::clone(&tally),
            }));
        }
        let frames = FramePool::new(
            BufferSize::new(geometry.payload(), geometry.slack()),
            memories,
        )?;

        let view = PoolView::attach(page_fd, pool_fds, &descs)?;
        let marks = Arc::new(Marks::create(geometry.buffers() as u32, slot, max_held)?);
        let doorbell = Doorbell::create()?;
        let (eventfd, bell_page) = doorbell.bell_fds()?;
        let marks_fd = marks.page_fd()?;

        // The reader's place in the ring, fixed here: the ring's write position is read now, and
        // the answer is sent right after, so nothing the producer publishes can sit between this
        // position and the reader's answer unseen.
        let mut ring = RingReader::attach(ring_fd)?;
        if start == Start::Newest {
            step_to_newest(&mut ring);
        }
        let reader = BufferReader::new(ring, FIELD, view, Arc::clone(&marks));

        let mut ack = [0_u8; ACK_LEN];
        ack[0..4].copy_from_slice(&VERSION.to_le_bytes());
        ack[4..8].copy_from_slice(&slot.to_le_bytes());
        fdpass::send_on(
            &socket,
            &[&ack],
            &[marks_fd.as_fd(), eventfd.as_fd(), bell_page.as_fd()],
        )?;

        Ok(Self {
            socket,
            buffers: reader,
            marks,
            frames,
            tally,
            doorbell,
            geometry,
            counters: Counters::default(),
        })
    }

    /// The next frame in publication order, held. `None` when nothing new is published, or when
    /// this reader is at its own limit on held buffers (counted as `limited`). Frames the ring
    /// lost before this reader got to them are counted as `skipped` and passed over.
    ///
    /// # Errors
    ///
    /// [`ReadError::Ended`] with [`StreamEnd::ConsumerFault`] once the producer has cut this reader off.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ReadError> {
        loop {
            // The end code is read before the ring: a ring that has ended has published every frame
            // before its end, and a read after this load sees them all.
            let ended = self.buffers.end();
            match self.buffers.next() {
                NextBatch::Batch(batch) => {
                    if let Some(frame) = admit(&self.frames, &mut self.counters, batch) {
                        return Ok(Some(frame));
                    }
                }
                NextBatch::Skipped(_) => {}
                NextBatch::Empty => {
                    return match StreamEnd::from_code(ended) {
                        None => Ok(None),
                        Some(end) => Err(ReadError::Ended(end)),
                    };
                }
                NextBatch::Limited => {
                    self.counters.limited += 1;
                    return Ok(None);
                }
                NextBatch::CutOff => return Err(ReadError::Ended(StreamEnd::ConsumerFault)),
                NextBatch::PoolChanged { generation } => {
                    return Err(ReadError::PoolChanged { generation });
                }
            }
        }
    }

    /// The newest frame published, holding it, and every older one passed over on the way
    /// (counted as `superseded`). The latest-only policy: a consumer that only wants the newest
    /// frame keeps its queue at zero.
    ///
    /// # Errors
    ///
    /// As [`Reader::next_frame`].
    pub fn latest(&mut self) -> Result<Option<Frame>, ReadError> {
        let mut newest: Option<Frame> = None;
        let mut budget = self.buffers.pending().saturating_add(1);
        while budget > 0 {
            budget -= 1;
            let frame = match self.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                // The frame held already is the newest the reader has. An end is sticky: the next
                // call reports it again, so it is not lost.
                Err(_) if newest.is_some() => break,
                Err(e) => return Err(e),
            };
            if let Some(older) = newest.replace(frame) {
                drop(older);
                self.counters.superseded += 1;
            }
        }
        Ok(newest)
    }

    /// Sleeps until the producer rings this reader's doorbell, unless a frame is already queued.
    ///
    /// # Errors
    ///
    /// If polling the doorbell fails.
    pub fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Wake> {
        let buffers = &self.buffers;
        // An ended ring is ready at once: the next read reports the end.
        self.doorbell
            .wait(timeout, || buffers.pending() > 0 || buffers.end() != 0)
    }

    /// Counts, now.
    pub fn stats(&self) -> ReaderStats {
        let counters = &self.counters;
        ReaderStats {
            delivered: counters.delivered,
            skipped: self.buffers.missed(),
            superseded: counters.superseded,
            limited: counters.limited,
            malformed: counters.malformed,
            held: self.marks.held(),
            queued: self.buffers.pending(),
            dma_brackets: self.tally.brackets.load(Ordering::Relaxed),
            dma_sync_errors: self.tally.errors.load(Ordering::Relaxed),
            age_last_ns: counters.age_last,
            age_max_ns: counters.age_max,
            age_mean_ns: counters
                .age_sum
                .checked_div(counters.delivered)
                .unwrap_or(0),
        }
    }

    /// The producer's shape, as its handshake named it.
    pub const fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// The socket this reader holds open. The producer counts the reader as alive while it is.
    pub const fn socket(&self) -> &UnixStream {
        &self.socket
    }
}

/// Moves a fresh ring reader, which starts at the write position, back to the oldest sample the
/// ring can still order, then forward over every published sample but the newest. Its next
/// sample is then the newest one published (none if nothing is published). The samples stepped
/// over are not frames the reader asked for, so they are not counted as lost. The steps are at
/// most one ring's window: the ring keeps no more than that to step over.
fn step_to_newest(ring: &mut RingReader) {
    ring.rewind();
    let mut budget = ring.geometry().window().saturating_add(1);
    while ring.pending() > 1 && budget > 0 {
        budget -= 1;
        // Each call moves the reader on, by one sample or by the samples it found gone.
        let _ = ring.next();
    }
}

/// One ring entry, made into a frame: its metadata decoded, its one buffer taken as a
/// `FrameLease` of this reader's pool. `None` (counted as malformed) for anything else; the
/// hold is then dropped at once.
fn admit(
    frames: &FramePool<()>,
    counters: &mut Counters,
    batch: Batch<'_, Arc<Marks>>,
) -> Option<Frame> {
    let Batch {
        seq,
        time,
        meta,
        held,
    } = batch;
    let decoded = decode_meta(meta);
    let user = decode_user(meta);
    let index = held.buffer(0).filter(|_| held.len() == 1);
    let (Some((id, format, layouts)), Some(user), Some(index)) = (decoded, user, index) else {
        counters.malformed += 1;
        return None;
    };
    let mut bytes = [0_u8; USER_MAX];
    bytes[..user.len()].copy_from_slice(user);
    let user = UserBytes {
        len: user.len(),
        bytes,
    };
    let entry = FrameEntry::new(id, time.as_nanos(), format, layouts);
    let Ok(lease) = frames.acquire_at(index.0 as usize, entry) else {
        counters.malformed += 1;
        return None;
    };
    let age_ns = SampleTime::now().saturating_since(time);
    counters.record(age_ns);
    Some(Frame {
        seq,
        time,
        age_ns,
        lease,
        held,
        user,
    })
}
