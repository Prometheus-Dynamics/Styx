//! Capture buffers and the frames that lend them out.
//!
//! Buffers are driver-allocated (`MMAP`, each also exported as a dma-buf with `VIDIOC_EXPBUF`)
//! or allocated from a dma-heap and imported (`DMABUF`). Either way every frame has a CPU
//! mapping and a dma-buf descriptor, so it can be read in place or handed to another process or
//! device without a copy. A frame returns its buffer to the queue when dropped.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use styx_kernel::dma_heap::{self, Access, DmaHeap};
use styx_kernel::v4l2::{BufType, Memory, QueueBuffer, VideoDevice};
use styx_kernel::{FourCc, Mapping};

use crate::control::FrameControls;
use crate::error::{KernelContext, NativeError, Result};

/// Where capture buffers come from.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum BufferMemory {
    /// Allocated by the driver and exported as dma-bufs.
    #[default]
    Mmap,
    /// Allocated from a dma-heap (`/dev/dma_heap/<name>`, e.g. `linux,cma`) and imported.
    DmaHeap(String),
}

struct Buffer {
    map: Mapping,
    dmabuf: Option<OwnedFd>,
}

/// The buffers of one stream. Freed (`REQBUFS 0`) when the last frame lending one is dropped.
pub(crate) struct BufferSet {
    buffers: Vec<Buffer>,
    video: Arc<VideoDevice>,
    buf_type: BufType,
    memory: Memory,
    len: usize,
    /// Frames currently lent out.
    outstanding: AtomicUsize,
}

impl BufferSet {
    /// Allocates and maps `count` buffers of `len` bytes (at least) on `video`.
    pub(crate) fn allocate(
        video: Arc<VideoDevice>,
        buf_type: BufType,
        memory: &BufferMemory,
        count: u32,
        len: usize,
    ) -> Result<Self> {
        let kmem = match memory {
            BufferMemory::Mmap => Memory::Mmap,
            BufferMemory::DmaHeap(_) => Memory::DmaBuf,
        };
        let got = video
            .request_buffers(buf_type, kmem, count.max(2))
            .step("VIDIOC_REQBUFS")?;
        let mut set = BufferSet {
            buffers: Vec::with_capacity(got.count as usize),
            video,
            buf_type,
            memory: kmem,
            len,
            outstanding: AtomicUsize::new(0),
        };
        match memory {
            BufferMemory::Mmap => {
                for i in 0..got.count {
                    let mut planes = set.video.map_buffer(buf_type, i).step("mmap buffer")?;
                    if planes.is_empty() {
                        return Err(NativeError::State("buffer without planes"));
                    }
                    let map = planes.remove(0);
                    // Export for sharing; drivers without EXPBUF still capture into the mapping.
                    let dmabuf = set.video.export_buffer(buf_type, i, 0).ok();
                    set.len = map.len();
                    set.buffers.push(Buffer { map, dmabuf });
                }
            }
            BufferMemory::DmaHeap(name) => {
                let heap = DmaHeap::open(name).step(&format!("open dma-heap {name}"))?;
                for _ in 0..got.count {
                    let buf = heap.allocate(len).step("allocate from dma-heap")?;
                    let map = buf.map().step("map dma-buf")?;
                    set.buffers.push(Buffer {
                        map,
                        dmabuf: Some(buf.into_fd()),
                    });
                }
            }
        }
        Ok(set)
    }

    pub(crate) fn count(&self) -> usize {
        self.buffers.len()
    }

    /// Bytes per buffer.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn memory(&self) -> Memory {
        self.memory
    }

    pub(crate) fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::Acquire)
    }

    /// Queues buffer `index`.
    pub(crate) fn queue(&self, index: u32) -> Result<()> {
        let b = self
            .buffers
            .get(index as usize)
            .ok_or(NativeError::State("no such buffer"))?;
        let req = match self.memory {
            Memory::DmaBuf => {
                let fd = b.dmabuf.as_ref().ok_or(NativeError::State("no dma-buf"))?;
                let mut q = QueueBuffer::dmabuf(self.buf_type, index, &[fd.as_fd()]);
                if let Some(p) = q.planes.first_mut() {
                    p.length = self.len as u32;
                }
                q
            }
            _ => QueueBuffer::mmap(self.buf_type, index),
        };
        self.video.queue(&req).step("VIDIOC_QBUF")
    }

    /// Queues every buffer.
    pub(crate) fn queue_all(&self) -> Result<()> {
        (0..self.buffers.len() as u32).try_for_each(|i| self.queue(i))
    }

    fn data(&self, index: u32) -> &[u8] {
        self.buffers[index as usize].map.as_slice()
    }

    fn dmabuf(&self, index: u32) -> Option<BorrowedFd<'_>> {
        self.buffers[index as usize]
            .dmabuf
            .as_ref()
            .map(|f| f.as_fd())
    }
}

impl Drop for BufferSet {
    fn drop(&mut self) {
        // Unmap and close the exports first, then free the queue's buffers.
        self.buffers.clear();
        let _ = self.video.free_buffers(self.buf_type, self.memory);
    }
}

/// What lets a frame give its buffer back: the buffers, and whether the stream still runs.
pub(crate) struct Lender {
    pub(crate) buffers: Arc<BufferSet>,
    pub(crate) streaming: Arc<AtomicBool>,
}

/// A captured frame. Dropping it returns its buffer to the capture queue.
pub struct NativeFrame {
    /// Frame sequence number from the receiver (starts at 0 with each stream start).
    pub sequence: u32,
    /// Capture timestamp (`CLOCK_MONOTONIC`, end of frame on most receivers).
    pub timestamp: Duration,
    /// When userspace dequeued the frame.
    pub dequeued: Instant,
    /// Payload bytes.
    pub bytes_used: usize,
    /// The receiver flagged the frame as corrupted.
    pub error: bool,
    /// Pixel format.
    pub fourcc: FourCc,
    /// Width in pixels.
    pub width: u32,
    /// Height in lines.
    pub height: u32,
    /// Bytes per line.
    pub stride: u32,
    /// Exposure, gain and frame duration that produced this frame.
    pub controls: Option<FrameControls>,
    index: u32,
    lender: Arc<Lender>,
    cpu_access: AtomicBool,
}

impl NativeFrame {
    pub(crate) fn new(
        index: u32,
        lender: Arc<Lender>,
        head: FrameHead,
        controls: Option<FrameControls>,
    ) -> Self {
        lender.buffers.outstanding.fetch_add(1, Ordering::AcqRel);
        NativeFrame {
            sequence: head.sequence,
            timestamp: head.timestamp,
            dequeued: Instant::now(),
            bytes_used: head.bytes_used,
            error: head.error,
            fourcc: head.fourcc,
            width: head.width,
            height: head.height,
            stride: head.stride,
            controls,
            index,
            lender,
            cpu_access: AtomicBool::new(false),
        }
    }

    /// The frame's bytes (`bytes_used` of them, or the whole buffer if the driver reported
    /// none). The first call brackets CPU access with a dma-buf sync.
    pub fn data(&self) -> &[u8] {
        if !self.cpu_access.swap(true, Ordering::AcqRel)
            && let Some(fd) = self.dmabuf()
        {
            let _ = dma_heap::sync(fd, Access::Read, true);
        }
        let all = self.lender.buffers.data(self.index);
        let n = if self.bytes_used == 0 {
            all.len()
        } else {
            self.bytes_used.min(all.len())
        };
        &all[..n]
    }

    /// The buffer's dma-buf descriptor (borrowed; `try_clone_to_owned` to keep it).
    pub fn dmabuf(&self) -> Option<BorrowedFd<'_>> {
        self.lender.buffers.dmabuf(self.index)
    }

    /// Size of the whole buffer in bytes.
    pub fn buffer_len(&self) -> usize {
        self.lender.buffers.len()
    }

    /// Buffer index in the capture queue.
    pub fn buffer_index(&self) -> u32 {
        self.index
    }
}

impl std::fmt::Debug for NativeFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFrame")
            .field("sequence", &self.sequence)
            .field("timestamp", &self.timestamp)
            .field("fourcc", &self.fourcc)
            .field("size", &(self.width, self.height))
            .field("stride", &self.stride)
            .field("bytes_used", &self.bytes_used)
            .field("error", &self.error)
            .field("controls", &self.controls)
            .finish_non_exhaustive()
    }
}

impl Drop for NativeFrame {
    fn drop(&mut self) {
        if self.cpu_access.load(Ordering::Acquire)
            && let Some(fd) = self.dmabuf()
        {
            let _ = dma_heap::sync(fd, Access::Read, false);
        }
        let set = &self.lender.buffers;
        if self.lender.streaming.load(Ordering::Acquire) {
            // Fails only when the stream stopped meanwhile; the buffer is then unqueued anyway.
            let _ = set.queue(self.index);
        }
        set.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The per-frame values read from a dequeued buffer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameHead {
    pub sequence: u32,
    pub timestamp: Duration,
    pub bytes_used: usize,
    pub error: bool,
    pub fourcc: FourCc,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}
