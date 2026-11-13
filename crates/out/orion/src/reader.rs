//! The Orion reader: a subscriber to an adopted buffer topic, given the frames as `FrameLease`s
//! of the producer's own memory, with no copy.
//!
//! The subscriber's pool is the one the publisher offered (`BufferSubscriber::pool`). Each of its
//! buffers is mapped read-only from the descriptor it sits in: a memfd or a dma-buf, which the
//! kind is read from the descriptor. A frame's ring entry names one buffer; the lease is that
//! buffer, and the hold is kept while the lease is.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use orion::Orion;
use orion::buffers::{
    Batch, BufferDesc, BufferIndex, BufferSubscriber, Held, HoldLimits, NextBatch, PoolView,
};
use orion::topics::Status;
use shm_ring_holds::Marks;
use styx_frame::{
    BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool, MemoryKind,
};

use crate::meta;
use crate::topic::OrionError;

/// A subscription to an adopted buffer topic. Frames come from its pool, once the publisher has
/// offered it.
pub struct Subscription {
    subscriber: BufferSubscriber,
    frames: Option<FramePool<()>>,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Subscription")
    }
}

impl std::fmt::Debug for OrionFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrionFrame")
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

/// One frame from an Orion subscription. Dropping it gives its buffer back to the publisher.
pub struct OrionFrame {
    seq: u64,
    /// Declared before `held`, so the pool's slot is free before the hold is taken off.
    lease: FrameLease<()>,
    held: Held<Arc<Marks>>,
}

impl OrionFrame {
    /// The ring's sequence number of the frame's entry.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// The frame: read its planes with [`FrameLease::read`].
    #[must_use]
    pub const fn lease(&self) -> &FrameLease<()> {
        &self.lease
    }

    /// Whether the entry names exactly one buffer, as every frame this crate publishes does.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.held.len() == 1
    }
}

/// Subscribes to the adopted topic `name`, holding at most `max_held` of its buffers. The
/// subscriber waits for a topic nobody has adopted yet.
///
/// # Errors
///
/// [`OrionError::Orion`] when the session is not on the board or the comm core refuses.
pub fn subscribe(orion: &Orion, name: &str, max_held: u32) -> Result<Subscription, OrionError> {
    let subscriber = orion.subscribe_buffers(
        name,
        &[],
        HoldLimits {
            max_held,
            max_hold_ms: 0,
        },
    )?;
    Ok(Subscription {
        subscriber,
        frames: None,
    })
}

impl Subscription {
    /// Where the topic is: waiting, live, or its publisher gone (withdrawn or dropped).
    #[must_use]
    pub fn status(&self) -> Status {
        self.subscriber.status()
    }

    /// Buffers in the pool the publisher offered; `None` until the offer has arrived.
    pub fn pool_buffers(&mut self) -> Option<usize> {
        self.subscriber.pool().map(|view| view.len() as usize)
    }

    /// Sleeps until a frame is published, for up to `wait`, on this subscriber's doorbell. `true`
    /// if there is a frame to read, `false` on timeout. Nothing is polled on a timer.
    ///
    /// # Errors
    ///
    /// [`OrionError::Orion`] when the connection broke.
    pub fn wait(&mut self, wait: Duration) -> Result<bool, OrionError> {
        Ok(self.subscriber.wait(wait)?)
    }

    /// The next frame in publication order, holding its buffer, or `None` when nothing new is
    /// published. Entries whose metadata this build does not read are skipped.
    ///
    /// # Errors
    ///
    /// [`OrionError::Frames`] when the pool cannot be mapped, or the publisher replaced its pool.
    pub fn next_frame(&mut self) -> Result<Option<OrionFrame>, OrionError> {
        if self.frames.is_none()
            && let Some(view) = self.subscriber.pool()
        {
            self.frames = Some(frame_pool(view)?);
        }
        loop {
            let frames = self.frames.as_ref();
            match self.subscriber.next() {
                NextBatch::Batch(batch) => {
                    if let Some(frames) = frames
                        && let Some(frame) = admit(frames, batch)
                    {
                        return Ok(Some(frame));
                    }
                }
                NextBatch::Skipped(_) => {}
                NextBatch::Empty | NextBatch::Limited => return Ok(None),
                NextBatch::CutOff => return Err(OrionError::Frames("cut off by the publisher")),
                NextBatch::PoolChanged { .. } => {
                    return Err(OrionError::Frames("the publisher replaced its pool"));
                }
            }
        }
    }
}

/// One entry, made into a frame: its metadata decoded, its one buffer taken as a lease of the
/// subscriber's pool. `None` for an entry this build does not read; the hold is then dropped.
fn admit(frames: &FramePool<()>, batch: Batch<'_, Arc<Marks>>) -> Option<OrionFrame> {
    let Batch {
        seq,
        time,
        meta,
        held,
    } = batch;
    let (id, format, layouts) = meta::decode(meta)?;
    if held.len() != 1 {
        return None;
    }
    let index = held.buffer(0)?;
    let entry = FrameEntry::new(id, time.as_nanos(), format, layouts);
    let lease = frames.acquire_at(index.0 as usize, entry).ok()?;
    Some(OrionFrame { seq, lease, held })
}

/// The subscriber's frame pool over the offered buffers: one read-only mapping per buffer, of the
/// kind its descriptor is. Every buffer is the same size, as the publisher's pool is.
fn frame_pool(view: &PoolView) -> Result<FramePool<()>, OrionError> {
    let count = view.len();
    if count == 0 {
        return Err(OrionError::Frames("the publisher offered no buffers"));
    }
    let mut memories: Vec<Box<dyn BufferMemory>> = Vec::with_capacity(count as usize);
    let mut size: Option<u64> = None;
    for index in 0..count {
        let (fd, desc) = view
            .descriptor(BufferIndex(index))
            .ok_or(OrionError::Frames(
                "a buffer of the offered pool is missing",
            ))?;
        let BufferDesc {
            offset,
            size: bytes,
            ..
        } = desc;
        if *size.get_or_insert(bytes) != bytes {
            return Err(OrionError::Frames("the offered buffers differ in size"));
        }
        let fd: OwnedFd = fd.try_clone_to_owned()?;
        let memory: Box<dyn BufferMemory> = match kind_of(&fd)? {
            MemoryKind::Memfd => Box::new(FdMemory::memfd_at(
                fd,
                offset as usize,
                bytes as usize,
                false,
            )?),
            MemoryKind::Dmabuf => Box::new(FdMemory::dmabuf_at(
                fd,
                offset as usize,
                bytes as usize,
                false,
            )?),
            _ => {
                return Err(OrionError::Frames(
                    "a buffer of the offered pool is not shared",
                ));
            }
        };
        memories.push(memory);
    }
    let size = size.unwrap_or(0) as usize;
    let size = BufferSize::new(size, 0);
    Ok(FramePool::new(size, memories)?)
}

/// The kind of memory a descriptor is, read from what it links to.
fn kind_of(fd: &OwnedFd) -> Result<MemoryKind, OrionError> {
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    let text = link.to_string_lossy();
    if text.contains("dmabuf") {
        Ok(MemoryKind::Dmabuf)
    } else if text.contains("memfd:") {
        Ok(MemoryKind::Memfd)
    } else {
        Err(OrionError::Frames(
            "a buffer's descriptor is not a memfd or a dma-buf",
        ))
    }
}
