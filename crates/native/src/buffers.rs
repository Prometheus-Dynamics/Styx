//! Capture buffers and the frames that lend them out.
//!
//! Buffers are driver-allocated (`MMAP`, each also exported as a dma-buf with `VIDIOC_EXPBUF`)
//! or allocated from a dma-heap and imported (`DMABUF`). Either way every frame has a CPU
//! mapping and a dma-buf descriptor, so it can be read in place or handed to another process or
//! device without a copy. A frame returns its buffer to the queue when dropped.
//!
//! Frames may outlive their stream. Stopping a stream releases the queue's buffers at once
//! (`REQBUFS 0`, which leaves buffers that are still mapped or exported to the memory that
//! backs them: "orphaned"), so the next stream, on this descriptor or another one, allocates
//! fresh buffers. A held frame keeps its mapping and dma-buf (and with them the memory) until
//! it is dropped; it is never queued again.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use styx_kernel::dma_heap::{self, Access, DmaBuf, DmaHeap};
use styx_kernel::v4l2::{Memory, QueueBuffer};
use styx_kernel::{FourCc, Mapping};

use crate::control::{FrameControls, lock};
use crate::device::CaptureDevice;
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

/// Buffers allocated outside the driver, imported as `DMABUF`.
pub(crate) enum Allocator {
    /// From a dma-heap.
    Heap(DmaHeap),
    /// memfds (tests).
    #[cfg(test)]
    Memfd,
}

impl Allocator {
    fn allocate(&self, len: usize) -> Result<DmaBuf> {
        match self {
            Allocator::Heap(h) => h.allocate(len).step("allocate from dma-heap"),
            #[cfg(test)]
            Allocator::Memfd => DmaBuf::memfd("styx-native-test", len).step("memfd"),
        }
    }
}

/// The buffers of one stream.
pub(crate) struct BufferSet {
    buffers: Vec<Buffer>,
    video: Arc<dyn CaptureDevice>,
    memory: Memory,
    len: usize,
    /// Frames currently lent out.
    outstanding: AtomicUsize,
    /// Whether frames go back to the queue when dropped. Cleared (under the lock, so no frame
    /// queues a buffer after) when the stream stops.
    live: Mutex<bool>,
    /// The queue no longer holds these buffers (`REQBUFS 0` done).
    released: AtomicBool,
}

impl BufferSet {
    /// Allocates and maps `count` buffers of `len` bytes (at least) on `video`: driver
    /// buffers without an allocator, imported ones with it.
    pub(crate) fn allocate_with(
        video: Arc<dyn CaptureDevice>,
        allocator: Option<Allocator>,
        count: u32,
        len: usize,
    ) -> Result<Self> {
        let kmem = if allocator.is_some() {
            Memory::DmaBuf
        } else {
            Memory::Mmap
        };
        let got = video
            .request_buffers(kmem, count.max(2))
            .step("VIDIOC_REQBUFS")?;
        // From here on dropping `set` frees the queue's buffers again.
        let mut set = BufferSet {
            buffers: Vec::with_capacity(got as usize),
            video,
            memory: kmem,
            len,
            outstanding: AtomicUsize::new(0),
            live: Mutex::new(true),
            released: AtomicBool::new(false),
        };
        if got == 0 {
            return Err(NativeError::State("the driver allocated no buffers"));
        }
        match &allocator {
            None => {
                for i in 0..got {
                    let map = set.video.map_buffer(i).step("mmap buffer")?;
                    // Export for sharing; drivers without EXPBUF still capture into the mapping.
                    let dmabuf = set.video.export_buffer(i).ok();
                    set.len = map.len();
                    set.buffers.push(Buffer { map, dmabuf });
                }
            }
            Some(alloc) => {
                for _ in 0..got {
                    let buf = alloc.allocate(len)?;
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
        let buf_type = self.video.buf_type();
        let req = match self.memory {
            Memory::DmaBuf => {
                let fd = b.dmabuf.as_ref().ok_or(NativeError::State("no dma-buf"))?;
                let mut q = QueueBuffer::dmabuf(buf_type, index, &[fd.as_fd()]);
                if let Some(p) = q.planes.first_mut() {
                    p.length = self.len as u32;
                }
                q
            }
            _ => QueueBuffer::mmap(buf_type, index),
        };
        self.video.queue(&req).step("VIDIOC_QBUF")
    }

    /// Queues every buffer.
    pub(crate) fn queue_all(&self) -> Result<()> {
        (0..self.buffers.len() as u32).try_for_each(|i| self.queue(i))
    }

    /// Whether frames still go back to the queue.
    pub(crate) fn is_live(&self) -> bool {
        *lock(&self.live)
    }

    /// Stops lending buffers back to the queue: frames dropped from now on keep their buffer
    /// out of it. Waits for a frame that is queueing its buffer right now.
    pub(crate) fn retire(&self) {
        *lock(&self.live) = false;
    }

    /// Frees the queue's buffers (`REQBUFS 0`) after [`Self::retire`] and `STREAMOFF`. Held
    /// frames keep their mappings and dma-bufs; the next stream allocates new buffers.
    pub(crate) fn release(&self) -> Result<()> {
        self.retire();
        if self.released.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.video
            .request_buffers(self.memory, 0)
            .map(|_| ())
            .step("VIDIOC_REQBUFS 0")
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

    /// Returns a lent buffer: queues it again while the stream runs.
    fn give_back(&self, index: u32) {
        {
            let live = lock(&self.live);
            if *live {
                // Fails only when the device went away; the stream reports that.
                let _ = self.queue(index);
            }
        }
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for BufferSet {
    fn drop(&mut self) {
        // Unmap and close the exports first, then free the queue's buffers unless that was
        // done when the stream stopped (it may hold a newer stream's buffers by now).
        self.buffers.clear();
        if !self.released.load(Ordering::Acquire) {
            let _ = self.video.request_buffers(self.memory, 0);
        }
    }
}

/// What lets a frame give its buffer back.
pub(crate) struct Lender {
    pub(crate) buffers: Arc<BufferSet>,
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
    /// none). The first call starts CPU (read) access with a dma-buf sync, which invalidates
    /// the CPU's cache over the buffer.
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
        // No `SYNC_END` for the read access `data` started: it only cleans the buffer's lines
        // for the device (arm64 `dcache_clean_poc` over the whole buffer, 22 us per 1.3 MB
        // frame on the CM5), and a CPU that only read them has none dirty. The next frame in
        // this buffer starts its own access, which invalidates what the CPU may have cached
        // meanwhile.
        self.lender.buffers.give_back(self.index);
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
