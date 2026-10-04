//! Capture buffers and the frames that lend them out.
//!
//! Buffers are driver-allocated (`MMAP`, each also exported as a dma-buf with `VIDIOC_EXPBUF`)
//! or allocated from a dma-heap and imported (`DMABUF`). Either way every frame has a CPU
//! mapping and a dma-buf descriptor, so it can be read in place or handed to another process or
//! device without a copy. A frame returns its buffer to the queue when dropped (the runtime's
//! buffer pool).
//!
//! Frames may outlive their stream. Stopping a stream releases the queue's buffers at once
//! (`REQBUFS 0`, which leaves buffers that are still mapped or exported to the memory that
//! backs them: "orphaned"), so the next stream, on this descriptor or another one, allocates
//! fresh buffers. A held frame keeps its mapping and dma-buf (and with them the memory) until
//! it is dropped; it is never queued again.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use styx_kernel::dma_heap::{self, DmaBuf, DmaHeap};
use styx_kernel::v4l2::{Memory, QueueBuffer};
use styx_kernel::{FourCc, Mapping};
use styx_runtime::styx_hal::{Access, FrameBuffer};

use crate::control::FrameControls;
use crate::device::CaptureDevice;
use crate::error::{KernelContext, NativeError, Result};
use crate::receiver::V4l2Receiver;

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
    /// memfds of this size whatever is asked for (tests: a heap that hands out too little).
    #[cfg(test)]
    MemfdOf(usize),
}

impl Allocator {
    fn allocate(&self, len: usize) -> Result<DmaBuf> {
        match self {
            Allocator::Heap(h) => h.allocate(len).step("allocate from dma-heap"),
            #[cfg(test)]
            Allocator::Memfd => DmaBuf::memfd("styx-native-test", len).step("memfd"),
            #[cfg(test)]
            Allocator::MemfdOf(n) => DmaBuf::memfd("styx-native-test", *n).step("memfd"),
        }
    }
}

/// Refuses a capture buffer smaller than the format's `sizeimage` (`need`): the receiver would
/// write past its end (vb2 checks imported buffers against the length userspace passes, which
/// is ours), into memory that is not the frame's.
fn check_len(what: &str, have: usize, need: usize) -> Result<()> {
    if have < need {
        return Err(NativeError::InvalidConfig(format!(
            "{what} of {have} bytes is smaller than the format's {need} bytes per frame"
        )));
    }
    Ok(())
}

/// The buffers of one stream: mappings and dma-bufs, kept until the last frame holding one is
/// dropped.
pub(crate) struct BufferSet {
    buffers: Vec<Buffer>,
    video: Arc<dyn CaptureDevice>,
    memory: Memory,
    len: usize,
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
            released: AtomicBool::new(false),
        };
        if got == 0 {
            return Err(NativeError::State("the driver allocated no buffers"));
        }
        match &allocator {
            None => {
                for i in 0..got {
                    let map = set.video.map_buffer(i).step("mmap buffer")?;
                    check_len("driver buffer", map.len(), len)?;
                    // Export for sharing; drivers without EXPBUF still capture into the mapping.
                    let dmabuf = set.video.export_buffer(i).ok();
                    set.len = map.len();
                    set.buffers.push(Buffer { map, dmabuf });
                }
            }
            Some(alloc) => {
                for _ in 0..got {
                    let buf = alloc.allocate(len)?;
                    // What the kernel allocated, not what was asked for: the receiver is told
                    // `len` at every QBUF and writes up to the format's `sizeimage`.
                    let size = dma_heap::dmabuf_size(buf.as_fd()).step("dma-buf size")?;
                    check_len("imported dma-buf", usize::try_from(size).unwrap_or(0), len)?;
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
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn memory(&self) -> Memory {
        self.memory
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

    /// Frees the queue's buffers (`REQBUFS 0`) after `STREAMOFF`. Held frames keep their
    /// mappings and dma-bufs; the next stream allocates new buffers.
    pub(crate) fn release(&self) -> Result<()> {
        if self.released.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.video
            .request_buffers(self.memory, 0)
            .map(|_| ())
            .step("VIDIOC_REQBUFS 0")
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

/// One capture buffer, as the runtime's pool lends it out: it keeps its stream's buffers (the
/// mappings and dma-bufs) alive while held.
pub struct V4l2Buffer {
    set: Arc<BufferSet>,
    index: u32,
}

impl V4l2Buffer {
    pub(crate) fn new(set: Arc<BufferSet>, index: u32) -> Option<Self> {
        (set.buffers.get(index as usize).is_some()).then_some(Self { set, index })
    }

    fn buffer(&self) -> &Buffer {
        &self.set.buffers[self.index as usize]
    }
}

impl FrameBuffer for V4l2Buffer {
    type Export<'a> = BorrowedFd<'a>;

    /// Size of the whole buffer in bytes.
    fn len(&self) -> usize {
        self.set.len
    }

    fn bytes(&self) -> &[u8] {
        self.buffer().map.as_slice()
    }

    /// A dma-buf sync (start) for reads: invalidates the CPU's cache over the buffer.
    fn begin_cpu(&self, access: Access) {
        if let Some(fd) = self.export() {
            let access = match access {
                Access::Read => dma_heap::Access::Read,
                Access::Write => dma_heap::Access::Write,
                Access::ReadWrite => dma_heap::Access::ReadWrite,
            };
            let _ = dma_heap::sync(fd, access, true);
        }
    }

    /// No `SYNC_END` after reads: it only cleans the buffer's lines for the device (arm64
    /// `dcache_clean_poc` over the whole buffer, 22 us per 1.3 MB frame on the CM5), and a CPU
    /// that only read them has none dirty.
    fn end_cpu(&self, access: Access) {
        if access != Access::Read
            && let Some(fd) = self.export()
        {
            let access = match access {
                Access::Write => dma_heap::Access::Write,
                _ => dma_heap::Access::ReadWrite,
            };
            let _ = dma_heap::sync(fd, access, false);
        }
    }

    fn export(&self) -> Option<BorrowedFd<'_>> {
        self.buffer().dmabuf.as_ref().map(|f| f.as_fd())
    }
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
    frame: styx_runtime::Frame<V4l2Receiver>,
}

impl NativeFrame {
    pub(crate) fn new(
        frame: styx_runtime::Frame<V4l2Receiver>,
        dequeued: Instant,
        fourcc: FourCc,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Self {
        NativeFrame {
            sequence: frame.sequence as u32,
            timestamp: styx_runtime::instant_duration(frame.timestamp),
            dequeued,
            bytes_used: frame.bytes_used,
            error: frame.corrupt,
            fourcc,
            width,
            height,
            stride,
            controls: frame.controls,
            frame,
        }
    }

    /// The frame's bytes (`bytes_used` of them, or the whole buffer if the driver reported
    /// none). The first call starts CPU (read) access with a dma-buf sync, which invalidates
    /// the CPU's cache over the buffer.
    pub fn data(&self) -> &[u8] {
        self.frame.data()
    }

    /// The buffer's dma-buf descriptor (borrowed; `try_clone_to_owned` to keep it).
    pub fn dmabuf(&self) -> Option<BorrowedFd<'_>> {
        self.frame.buffer().export()
    }

    /// Size of the whole buffer in bytes.
    pub fn buffer_len(&self) -> usize {
        self.frame.buffer().len()
    }

    /// Buffer index in the capture queue.
    pub fn buffer_index(&self) -> u32 {
        self.frame.index()
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
