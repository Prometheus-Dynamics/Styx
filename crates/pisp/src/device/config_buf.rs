//! The buffer a config node (e.g. `pispbe-config`) reads its per-job config from.
//!
//! The driver copies the config out of the buffer with the CPU when it is queued
//! (`pispbe` copies the whole 16.7 KB `pisp_be_tiles_config`). A V4L2 MMAP buffer is mapped
//! uncached, so that copy reads uncached memory: 130 µs per job on the CM5. A buffer from a
//! cached dma-heap (imported with `V4L2_MEMORY_DMABUF`) is read from the cache instead; the
//! CPU is the only one that touches it, so no cache maintenance is needed for the copy.
//!
//! There are a few buffers, used in turn, so several jobs can be queued at once (the extra
//! back end passes go back to back); jobs finish in the order they were queued.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use styx_kernel::Mapping;
use styx_kernel::dma_heap::{DmaBuf, DmaHeap};
use styx_kernel::v4l2::{BufType, Memory, Planes, QueueBuffer, QueuePlane, VideoDevice};

use super::{DeviceError, Queue, Result};

/// Where a config buffer comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConfigMemory {
    /// A cached dma-heap (`linux,cma`, else `system`), else the driver's own buffers.
    #[default]
    CachedHeap,
    /// The driver's own buffers (V4L2 MMAP, mapped uncached).
    Driver,
}

const HEAPS: [&str; 2] = ["linux,cma", "system"];

fn default_heaps(memory: ConfigMemory) -> Vec<&'static str> {
    match memory {
        ConfigMemory::CachedHeap => HEAPS.to_vec(),
        ConfigMemory::Driver => Vec::new(),
    }
}

/// Config buffers, from a dma-heap or the driver, used in turn.
pub(super) struct ConfigBuffer {
    mem: ConfigMem,
    /// The buffer the next job writes and queues.
    next: usize,
    /// Jobs queued and not yet dequeued.
    queued: usize,
}

enum ConfigMem {
    Mmap(Queue),
    Heap {
        dev: VideoDevice,
        buf_type: BufType,
        bufs: Vec<(DmaBuf, Mapping)>,
        name: &'static str,
    },
}

impl ConfigBuffer {
    /// `count` buffers of at least `len` bytes each on `dev` (format already set).
    pub(super) fn new(
        dev: VideoDevice,
        buf_type: BufType,
        len: usize,
        count: usize,
        name: &'static str,
        memory: ConfigMemory,
    ) -> Result<Self> {
        let heaps = default_heaps(memory);
        let len = len.next_multiple_of(4096);
        let count = count.max(1);
        for heap in heaps {
            let Ok(heap) = DmaHeap::open(heap) else {
                continue;
            };
            let bufs: Option<Vec<_>> = (0..count)
                .map(|_| {
                    let buf = heap.allocate(len).ok()?;
                    let map = buf.map().ok()?;
                    Some((buf, map))
                })
                .collect();
            let Some(bufs) = bufs else { continue };
            match dev.request_buffers(buf_type, Memory::DmaBuf, count as u32) {
                Ok(got) if got.count as usize >= count => {
                    return Ok(Self::with(ConfigMem::Heap {
                        dev,
                        buf_type,
                        bufs,
                        name,
                    }));
                }
                _ => {
                    let _ = dev.free_buffers(buf_type, Memory::DmaBuf);
                }
            }
        }
        Ok(Self::with(ConfigMem::Mmap(Queue::new(
            dev,
            buf_type,
            count as u32,
            name,
        )?)))
    }

    fn with(mem: ConfigMem) -> Self {
        Self {
            mem,
            next: 0,
            queued: 0,
        }
    }

    fn count(&self) -> usize {
        match &self.mem {
            ConfigMem::Mmap(q) => q.maps.len(),
            ConfigMem::Heap { bufs, .. } => bufs.len(),
        }
    }

    /// Whether another job can be queued (a buffer is not in flight).
    pub(super) fn free(&self) -> bool {
        self.queued < self.count()
    }

    /// Where the buffers come from: `mmap` or the heap's name.
    pub(super) fn source(&self) -> String {
        match &self.mem {
            ConfigMem::Mmap(_) => "mmap".into(),
            ConfigMem::Heap { bufs, .. } => {
                format!("dma-heap ({} x {} bytes)", bufs.len(), bufs[0].0.len())
            }
        }
    }

    /// Copies `bytes` to the start of the next job's buffer (check [`Self::free`] first).
    pub(super) fn write(&mut self, bytes: &[u8]) {
        let next = self.next;
        let dst = match &mut self.mem {
            ConfigMem::Mmap(q) => q.maps[next][0].as_mut_slice(),
            ConfigMem::Heap { bufs, .. } => bufs[next].1.as_mut_slice(),
        };
        dst[..bytes.len()].copy_from_slice(bytes);
    }

    /// Queues the next job's buffer with `used` bytes of payload.
    pub(super) fn queue(&mut self, used: u32) -> Result<()> {
        let next = self.next;
        match &self.mem {
            ConfigMem::Mmap(q) => q.queue(next as u32, &[used])?,
            ConfigMem::Heap {
                dev,
                buf_type,
                bufs,
                name,
            } => {
                let buf = &bufs[next].0;
                let mut q = QueueBuffer::dmabuf(*buf_type, next as u32, &[buf.as_fd()]);
                q.planes = Planes::one(QueuePlane {
                    dmabuf: Some(buf.as_fd()),
                    length: buf.len() as u32,
                    bytes_used: used,
                    data_offset: 0,
                });
                super::profile::time(name, "qbuf", || dev.queue(&q))?;
            }
        }
        self.next = (next + 1) % self.count();
        self.queued += 1;
        Ok(())
    }

    /// Waits up to `timeout` for the oldest queued buffer to come back.
    pub(super) fn dequeue(&mut self, timeout: Duration) -> Result<()> {
        self.dequeue_mem(timeout)?;
        self.queued = self.queued.saturating_sub(1);
        Ok(())
    }

    fn dequeue_mem(&self, timeout: Duration) -> Result<()> {
        match &self.mem {
            ConfigMem::Mmap(q) => q.dequeue(timeout).map(|_| ()),
            ConfigMem::Heap {
                dev,
                buf_type,
                name,
                ..
            } => {
                let deadline = Instant::now() + timeout;
                loop {
                    if super::profile::time(name, "dqbuf", || {
                        dev.dequeue(*buf_type, Memory::DmaBuf)
                    })?
                    .is_some()
                    {
                        return Ok(());
                    }
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(DeviceError::Timeout(name));
                    }
                    dev.wait(Some(left))?;
                }
            }
        }
    }

    /// Starts streaming.
    pub(super) fn stream_on(&self) -> Result<()> {
        match &self.mem {
            ConfigMem::Mmap(q) => Ok(q.stream_on()?),
            ConfigMem::Heap { dev, buf_type, .. } => Ok(dev.stream_on(*buf_type)?),
        }
    }

    /// Stops streaming and frees the buffers.
    pub(super) fn close(self) -> Result<()> {
        match self.mem {
            ConfigMem::Mmap(q) => q.close(),
            ConfigMem::Heap {
                dev,
                buf_type,
                bufs,
                ..
            } => {
                let r = dev.stream_off(buf_type);
                drop(bufs);
                let f = dev.free_buffers(buf_type, Memory::DmaBuf);
                r?;
                f?;
                Ok(())
            }
        }
    }
}
