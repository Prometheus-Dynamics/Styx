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
use styx_kernel::v4l2::{BufType, DequeuedBuffer, Memory, QueueBuffer, QueuePlane, VideoDevice};

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
}

/// Environment variable overriding where output buffers come from: `driver` or `cached`
/// (for measurements).
pub const OUTPUT_MEMORY_ENV: &str = "STYX_PISP_OUTPUT_MEMORY";

/// The heaps [`OutputMemory::CachedHeap`] tries, in order.
const CACHED_HEAPS: [&str; 2] = ["linux,cma", "system"];

/// One output node's buffers, all mapped and exported.
pub(super) struct OutputQueue {
    dev: VideoDevice,
    memory: Memory,
    name: &'static str,
    maps: Vec<Mapping>,
    fds: Vec<OwnedFd>,
    len: u32,
}

const TYPE: BufType = BufType::VideoCaptureMplane;

impl OutputQueue {
    /// `count` buffers of at least `size` bytes on `dev` (format set).
    pub(super) fn new(
        dev: VideoDevice,
        memory: OutputMemory,
        count: u32,
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
        match (memory, heaps.is_empty()) {
            (OutputMemory::CachedHeap, false) => {
                let size = size.next_multiple_of(4096);
                let mut maps = Vec::new();
                let mut fds = Vec::new();
                for _ in 0..count {
                    // Contiguous memory first; the system heap when it runs out (the CM5 has
                    // 64 MiB of it, which many buffers of extra passes can exhaust).
                    let mut last = None;
                    let buf = heaps.iter().find_map(|heap| match heap.allocate(size) {
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
                            return Err(DeviceError::Setup(format!("{name}: no dma-heap")));
                        }
                    };
                    maps.push(buf.map()?);
                    fds.push(buf.into_fd());
                }
                let got = dev.request_buffers(TYPE, Memory::DmaBuf, count)?;
                if (got.count as usize) < fds.len() {
                    let _ = dev.free_buffers(TYPE, Memory::DmaBuf);
                    return Err(DeviceError::Setup(format!(
                        "{name}: {} of {count} buffer slots",
                        got.count
                    )));
                }
                Ok(Self {
                    dev,
                    memory: Memory::DmaBuf,
                    name,
                    maps,
                    fds,
                    len: size as u32,
                })
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
                    maps,
                    fds,
                    len,
                })
            }
        }
    }

    /// Number of buffers.
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
            q.planes = vec![QueuePlane::default()];
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
