//! The buffer a config node (e.g. `pispbe-config`) reads its per-job config from.
//!
//! The driver copies the config out of the buffer with the CPU when it is queued
//! (`pispbe` copies the whole 16.7 KB `pisp_be_tiles_config`). A V4L2 MMAP buffer is mapped
//! uncached, so that copy reads uncached memory: 130 µs per job on the CM5. A buffer from a
//! cached dma-heap (imported with `V4L2_MEMORY_DMABUF`) is read from the cache instead; the
//! CPU is the only one that touches it, so no cache maintenance is needed for the copy.

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

/// One config buffer, from a dma-heap or the driver.
pub(super) enum ConfigBuffer {
    Mmap(Queue),
    Heap {
        dev: VideoDevice,
        buf_type: BufType,
        buf: DmaBuf,
        map: Mapping,
        name: &'static str,
    },
}

impl ConfigBuffer {
    /// One buffer of at least `len` bytes on `dev` (format already set).
    pub(super) fn new(
        dev: VideoDevice,
        buf_type: BufType,
        len: usize,
        name: &'static str,
        memory: ConfigMemory,
    ) -> Result<Self> {
        let heaps = default_heaps(memory);
        let len = len.next_multiple_of(4096);
        for heap in heaps {
            let Ok(buf) = DmaHeap::open(heap).and_then(|h| h.allocate(len)) else {
                continue;
            };
            let Ok(map) = buf.map() else { continue };
            match dev.request_buffers(buf_type, Memory::DmaBuf, 1) {
                Ok(got) if got.count >= 1 => {
                    return Ok(Self::Heap {
                        dev,
                        buf_type,
                        buf,
                        map,
                        name,
                    });
                }
                _ => {
                    let _ = dev.free_buffers(buf_type, Memory::DmaBuf);
                }
            }
        }
        Ok(Self::Mmap(Queue::new(dev, buf_type, 1, name)?))
    }

    /// Where the buffer comes from: `mmap` or the heap's name.
    pub(super) fn source(&self) -> String {
        match self {
            Self::Mmap(_) => "mmap".into(),
            Self::Heap { buf, .. } => format!("dma-heap ({} bytes)", buf.len()),
        }
    }

    /// Copies `bytes` to the start of the buffer.
    pub(super) fn write(&mut self, bytes: &[u8]) {
        let dst = match self {
            Self::Mmap(q) => q.maps[0][0].as_mut_slice(),
            Self::Heap { map, .. } => map.as_mut_slice(),
        };
        dst[..bytes.len()].copy_from_slice(bytes);
    }

    /// Queues the buffer with `used` bytes of payload.
    pub(super) fn queue(&self, used: u32) -> Result<()> {
        match self {
            Self::Mmap(q) => q.queue(0, &[used]),
            Self::Heap {
                dev,
                buf_type,
                buf,
                name,
                ..
            } => {
                let mut q = QueueBuffer::dmabuf(*buf_type, 0, &[buf.as_fd()]);
                q.planes = Planes::one(QueuePlane {
                    dmabuf: Some(buf.as_fd()),
                    length: buf.len() as u32,
                    bytes_used: used,
                    data_offset: 0,
                });
                super::profile::time(name, "qbuf", || dev.queue(&q))?;
                Ok(())
            }
        }
    }

    /// Waits up to `timeout` for the buffer to come back.
    pub(super) fn dequeue(&self, timeout: Duration) -> Result<()> {
        match self {
            Self::Mmap(q) => q.dequeue(timeout).map(|_| ()),
            Self::Heap {
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
        match self {
            Self::Mmap(q) => q.stream_on(),
            Self::Heap { dev, buf_type, .. } => Ok(dev.stream_on(*buf_type)?),
        }
    }

    /// Stops streaming and frees the buffer.
    pub(super) fn close(self) -> Result<()> {
        match self {
            Self::Mmap(q) => q.close(),
            Self::Heap {
                dev, buf_type, map, ..
            } => {
                let r = dev.stream_off(buf_type);
                drop(map);
                let f = dev.free_buffers(buf_type, Memory::DmaBuf);
                r?;
                f?;
                Ok(())
            }
        }
    }
}
