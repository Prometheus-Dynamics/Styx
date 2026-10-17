//! The buffers of a back end output: the driver's (MMAP, mapped uncached) or dma-bufs from a
//! cached dma-heap (imported with `V4L2_MEMORY_DMABUF`).
//!
//! The driver's buffers cost no cache maintenance, which suits outputs that go to other
//! hardware (encoders, the GPU, other processes passing them on); the CPU reads them at
//! uncached speed (1.9 ms for the Y plane of 1280x800 on the CM5). Cached heap buffers are
//! read at memory speed, for the price of cache maintenance on every job (the kernel cleans
//! them when queued and invalidates them when dequeued).

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use styx_kernel::Mapping;
use styx_kernel::dma_heap::DmaHeap;
use styx_kernel::v4l2::{
    BufType, DequeuedBuffer, Memory, Planes, QueueBuffer, QueuePlane, VideoDevice,
};

use super::{DeviceError, Result};

/// Where a back end output's buffers come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMemory {
    /// The driver's buffers (V4L2 MMAP): no cache maintenance, uncached CPU reads.
    #[default]
    Driver,
    /// Buffers from a cached dma-heap (`linux,cma`, else, or when it runs out, `system`):
    /// cached CPU reads, cache maintenance per job.
    CachedHeap,
    /// As [`Self::CachedHeap`], with at most this many buffers per output from contiguous
    /// memory (`linux,cma`) and the rest from the `system` heap first, so a capture with many
    /// buffers leaves contiguous memory to the front end and other devices.
    CachedHeapContiguousUpTo(u32),
}

/// Environment variable overriding where output buffers come from: `driver` or `cached`
/// (for measurements).
pub const OUTPUT_MEMORY_ENV: &str = "STYX_PISP_OUTPUT_MEMORY";

/// The heaps [`OutputMemory::CachedHeap`] tries, in order.
const CACHED_HEAPS: [&str; 2] = ["linux,cma", "system"];

/// One output node's buffers, all mapped and exported. Cached heap buffers can start fewer than
/// the node's buffer slots and grow into them ([`OutputQueue::grow`]).
pub(super) struct OutputQueue {
    dev: VideoDevice,
    memory: Memory,
    name: &'static str,
    maps: Vec<Mapping>,
    fds: Vec<OwnedFd>,
    len: u32,
    /// Buffer slots the node has (`maps.len()` up to this).
    slots: usize,
    /// Where more cached buffers come from.
    heaps: Vec<DmaHeap>,
}

const TYPE: BufType = BufType::VideoCaptureMplane;

impl OutputQueue {
    /// `count` buffer slots for buffers of at least `size` bytes on `dev` (format set): cached
    /// heap buffers for `initial` of them now, the rest as [`Self::grow`] asks; the driver's
    /// buffers all at once.
    pub(super) fn new(
        dev: VideoDevice,
        memory: OutputMemory,
        count: u32,
        initial: u32,
        size: usize,
        name: &'static str,
    ) -> Result<Self> {
        let memory = match std::env::var(OUTPUT_MEMORY_ENV).as_deref() {
            Ok("driver") => OutputMemory::Driver,
            Ok("cached") => OutputMemory::CachedHeap,
            _ => memory,
        };
        let heaps: Vec<DmaHeap> = CACHED_HEAPS
            .iter()
            .filter_map(|h| DmaHeap::open(h).ok())
            .collect();
        let contiguous = match memory {
            OutputMemory::CachedHeapContiguousUpTo(n) => n,
            _ => u32::MAX,
        };
        match (memory, heaps.is_empty()) {
            (OutputMemory::CachedHeap | OutputMemory::CachedHeapContiguousUpTo(_), false) => {
                let got = dev.request_buffers(TYPE, Memory::DmaBuf, count)?;
                if got.count < count {
                    let _ = dev.free_buffers(TYPE, Memory::DmaBuf);
                    return Err(DeviceError::Setup(format!(
                        "{name}: {} of {count} buffer slots",
                        got.count
                    )));
                }
                let mut queue = Self {
                    dev,
                    memory: Memory::DmaBuf,
                    name,
                    maps: Vec::new(),
                    fds: Vec::new(),
                    len: size.next_multiple_of(4096) as u32,
                    slots: count as usize,
                    heaps,
                };
                for i in 0..initial.clamp(1, count) {
                    // Contiguous memory first, the system heap for the buffers beyond
                    // `contiguous`.
                    queue.allocate(i >= contiguous)?;
                }
                Ok(queue)
            }
            // The driver's buffers, also when no cached heap exists.
            _ => {
                let got = dev.request_buffers(TYPE, Memory::Mmap, count)?;
                if got.count == 0 {
                    return Err(DeviceError::Setup(format!("{name}: no buffers")));
                }
                let mut maps = Vec::new();
                let mut fds = Vec::new();
                for i in 0..got.count {
                    let mut planes = dev.map_buffer(TYPE, i)?;
                    if planes.is_empty() {
                        return Err(DeviceError::Setup(format!(
                            "{name}: buffer {i} has no plane"
                        )));
                    }
                    maps.push(planes.remove(0));
                    fds.push(dev.export_buffer(TYPE, i, 0)?);
                }
                let len = maps[0].len() as u32;
                Ok(Self {
                    dev,
                    memory: Memory::Mmap,
                    name,
                    slots: maps.len(),
                    maps,
                    fds,
                    len,
                    heaps: Vec::new(),
                })
            }
        }
    }

    /// One more cached heap buffer: contiguous memory first, the system heap when it runs out
    /// (the CM5 has 64 MiB of it, which many buffers of extra passes can exhaust).
    fn allocate(&mut self, system_first: bool) -> Result<u32> {
        let size = self.len as usize;
        let mut last = None;
        let mut order: Vec<&DmaHeap> = self.heaps.iter().collect();
        if system_first {
            order.reverse();
        }
        let buf = order.iter().find_map(|heap| match heap.allocate(size) {
            Ok(buf) => Some(buf),
            Err(e) => {
                last = Some(e);
                None
            }
        });
        let buf = match (buf, last) {
            (Some(buf), _) => buf,
            (None, Some(e)) => return Err(e.into()),
            (None, None) => {
                return Err(DeviceError::Setup(format!("{}: no dma-heap", self.name)));
            }
        };
        self.maps.push(buf.map()?);
        self.fds.push(buf.into_fd());
        Ok(self.maps.len() as u32 - 1)
    }

    /// A new buffer in a free slot (cached heap buffers only): its index, `None` when every
    /// slot has one or the memory ran out.
    pub(super) fn grow(&mut self) -> Option<u32> {
        if self.memory != Memory::DmaBuf || self.maps.len() >= self.slots {
            return None;
        }
        // The system heap first: contiguous memory can take 10-20 ms to allocate when it is
        // short (compaction), too long between two frames.
        super::profile::time(self.name, "grow", || self.allocate(true)).ok()
    }

    /// Whether the buffers come from a cached dma-heap (CPU reads at memory speed) rather than
    /// the driver (uncached).
    pub(super) fn cached(&self) -> bool {
        self.memory == Memory::DmaBuf
    }

    pub(super) fn len(&self) -> usize {
        self.maps.len()
    }

    /// Buffer `index`'s bytes.
    pub(super) fn data(&self, index: u32) -> Option<&[u8]> {
        self.maps.get(index as usize).map(Mapping::as_slice)
    }

    /// Buffer `index`'s dma-buf.
    pub(super) fn dmabuf(&self, index: u32) -> Option<BorrowedFd<'_>> {
        self.fds.get(index as usize).map(AsFd::as_fd)
    }

    /// Queues buffer `index` for the next job.
    pub(super) fn queue(&self, index: u32) -> Result<()> {
        let mut q = match self.memory {
            Memory::DmaBuf => {
                let fd = self.dmabuf(index).ok_or_else(|| {
                    DeviceError::Setup(format!("{}: no buffer {index}", self.name))
                })?;
                let mut q = QueueBuffer::dmabuf(TYPE, index, &[fd]);
                q.planes[0].length = self.len;
                q
            }
            _ => QueueBuffer::mmap(TYPE, index),
        };
        if q.planes.is_empty() {
            q.planes = Planes::one(QueuePlane::default());
        }
        super::profile::time(self.name, "qbuf", || self.dev.queue(&q))?;
        Ok(())
    }

    /// Waits up to `timeout` for a finished buffer.
    pub(super) fn dequeue(&self, timeout: Duration) -> Result<DequeuedBuffer> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(b) =
                super::profile::time(self.name, "dqbuf", || self.dev.dequeue(TYPE, self.memory))?
            {
                return Ok(b);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(DeviceError::Timeout(self.name));
            }
            super::profile::time(self.name, "poll", || self.dev.wait(Some(left)))?;
        }
    }

    /// Starts streaming.
    /// The node's descriptor: readable when a job's buffer is done.
    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.dev.as_fd()
    }

    pub(super) fn stream_on(&self) -> Result<()> {
        Ok(self.dev.stream_on(TYPE)?)
    }

    /// Stops streaming and frees the buffers (mappings and exports dropped first).
    pub(super) fn close(mut self) -> Result<()> {
        let r = self.dev.stream_off(TYPE);
        self.maps.clear();
        self.fds.clear();
        let f = self.dev.free_buffers(TYPE, self.memory);
        r?;
        f?;
        Ok(())
    }
}
