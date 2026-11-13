//! The reader: a process that takes the producer's frames in place.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use shm_fdpass::{self as fdpass, MAX_FDS};
use shm_ring::{FieldId, ReadMap, Reader as RingReader, SampleTime};
use shm_ring_buffers::{Batch, BufferDesc, BufferReader, Held, MAX_BUFFERS, NextBatch, PoolView};
use shm_ring_doorbell::{Doorbell, Wake};
use shm_ring_holds::Marks;
use styx_frame::{BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool};

use crate::SocketError;
use crate::wire::{ACK_LEN, CHUNK_LEN, OFFER_LEN, VERSION, decode_meta};

const FIELD: FieldId = FieldId(0);

/// Why [`Reader::next_frame`] stopped delivering frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// The producer cut this reader off for breaking its limits. Frames it holds may change, so
    /// it takes no more. Drop the reader and connect again.
    CutOff,
    /// The producer replaced its pool. This reader does not have the new buffers.
    PoolChanged {
        /// The new pool's generation.
        generation: u32,
    },
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CutOff => f.write_str("the producer cut this reader off for its limits"),
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
}

impl Frame {
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

/// A reader of one producer's frames.
#[derive(Debug)]
pub struct Reader {
    /// Kept open: the producer counts the reader as alive while this socket is.
    socket: UnixStream,
    buffers: BufferReader<ReadMap, Arc<Marks>>,
    marks: Arc<Marks>,
    frames: FramePool<()>,
    doorbell: Doorbell,
    counters: Counters,
}

impl Reader {
    /// Takes the producer's offer on `socket`, maps the buffers once, and answers with this
    /// reader's marks and doorbell. Blocks until the producer's offer arrives.
    ///
    /// # Errors
    ///
    /// [`SocketError`] if the offer is not the one this build understands, or a mapping fails.
    pub fn connect(socket: UnixStream) -> Result<Self, SocketError> {
        let mut offer = [0_u8; OFFER_LEN];
        let received = fdpass::recv(socket.as_fd(), &mut offer, MAX_FDS, false)?.ok_or(
            SocketError::Protocol("the producer closed before its offer"),
        )?;
        if received.len != OFFER_LEN || offer[0..4] != VERSION.to_le_bytes() {
            return Err(SocketError::Protocol(
                "the offer is not this protocol version",
            ));
        }
        let buffers = u32::from_le_bytes(offer[4..8].try_into().unwrap_or([0; 4])) as usize;
        let slot = u32::from_le_bytes(offer[8..12].try_into().unwrap_or([0; 4]));
        let max_held = u32::from_le_bytes(offer[12..16].try_into().unwrap_or([0; 4]));
        let payload = usize::try_from(u64::from_le_bytes(
            offer[16..24].try_into().unwrap_or([0; 8]),
        ))
        .map_err(|_| SocketError::Protocol("a buffer is longer than this build can map"))?;
        if buffers == 0 || buffers > MAX_BUFFERS as usize {
            return Err(SocketError::Protocol("the offer names no usable buffers"));
        }
        if payload == 0 {
            return Err(SocketError::Protocol("the offer's buffers are empty"));
        }

        let mut fds = received.fds;
        let first_count = fds.len();
        if first_count < 2 || first_count - 2 > buffers {
            return Err(SocketError::Protocol(
                "the offer carries the wrong descriptors",
            ));
        }
        let (Some(ring_fd), Some(page_fd)) = (fds.take(0), fds.take(1)) else {
            return Err(SocketError::Protocol(
                "the offer lacks the ring or the pool page",
            ));
        };
        let mut descriptors: Vec<OwnedFd> = Vec::with_capacity(buffers);
        for index in 2..first_count {
            descriptors.push(
                fds.take(index)
                    .ok_or(SocketError::Protocol("a buffer descriptor is missing"))?,
            );
        }
        while descriptors.len() < buffers {
            let mut chunk = [0_u8; CHUNK_LEN];
            let received = fdpass::recv(socket.as_fd(), &mut chunk, MAX_FDS, false)?.ok_or(
                SocketError::Protocol("the producer closed inside the buffers"),
            )?;
            let first = u32::from_le_bytes(chunk[4..8].try_into().unwrap_or([0; 4])) as usize;
            if received.len != CHUNK_LEN
                || chunk[0..4] != VERSION.to_le_bytes()
                || first != descriptors.len()
            {
                return Err(SocketError::Protocol("a buffer message is out of order"));
            }
            let mut fds = received.fds;
            let count = fds.len();
            if count == 0 || descriptors.len() + count > buffers {
                return Err(SocketError::Protocol(
                    "a buffer message carries the wrong count",
                ));
            }
            for index in 0..count {
                descriptors.push(
                    fds.take(index)
                        .ok_or(SocketError::Protocol("a buffer descriptor is missing"))?,
                );
            }
        }

        // The ring's own view of the buffers: read-only, checked by the ring crate.
        let descs: Vec<BufferDesc> = (0..buffers)
            .map(|index| BufferDesc {
                descriptor: index as u8,
                offset: 0,
                size: payload as u64,
            })
            .collect();
        let view_fds = descriptors
            .iter()
            .map(OwnedFd::try_clone)
            .collect::<io::Result<Vec<OwnedFd>>>()?;
        let view = PoolView::attach(page_fd, view_fds, &descs)?;
        let ring = RingReader::attach(ring_fd)?;
        let marks = Arc::new(Marks::create(buffers as u32, slot, max_held)?);
        let reader = BufferReader::new(ring, FIELD, view, Arc::clone(&marks));

        // Our own FrameLease view of the same buffers, mapped read-only once.
        let mut memories: Vec<Box<dyn BufferMemory>> = Vec::with_capacity(buffers);
        for fd in descriptors {
            memories.push(Box::new(FdMemory::memfd(fd, payload, false)?));
        }
        let frames = FramePool::new(BufferSize::new(payload, 0), memories)?;

        let doorbell = Doorbell::create()?;
        let (eventfd, bell_page) = doorbell.bell_fds()?;
        let marks_fd = marks.page_fd()?;
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
            doorbell,
            counters: Counters::default(),
        })
    }

    /// The next frame in publication order, held. `None` when nothing new is published, or when
    /// this reader is at its own limit on held buffers (counted as `limited`). Frames the ring
    /// lost before this reader got to them are counted as `skipped` and passed over.
    ///
    /// # Errors
    ///
    /// [`ReadError::CutOff`] once the producer has cut this reader off.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ReadError> {
        loop {
            match self.buffers.next() {
                NextBatch::Batch(batch) => {
                    if let Some(frame) = admit(&self.frames, &mut self.counters, batch) {
                        return Ok(Some(frame));
                    }
                }
                NextBatch::Skipped(_) => {}
                NextBatch::Empty => return Ok(None),
                NextBatch::Limited => {
                    self.counters.limited += 1;
                    return Ok(None);
                }
                NextBatch::CutOff => return Err(ReadError::CutOff),
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
            let Some(frame) = self.next_frame()? else {
                break;
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
        self.doorbell.wait(timeout, || buffers.pending() > 0)
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
            age_last_ns: counters.age_last,
            age_max_ns: counters.age_max,
            age_mean_ns: counters
                .age_sum
                .checked_div(counters.delivered)
                .unwrap_or(0),
        }
    }

    /// The socket this reader holds open. The producer counts the reader as alive while it is.
    pub const fn socket(&self) -> &UnixStream {
        &self.socket
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
    let index = held.buffer(0).filter(|_| held.len() == 1);
    let (Some((id, format, layouts)), Some(index)) = (decoded, index) else {
        counters.malformed += 1;
        return None;
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
    })
}
