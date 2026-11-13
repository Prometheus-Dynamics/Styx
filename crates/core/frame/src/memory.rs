//! The memory a buffer lives in.

use alloc::boxed::Box;
use alloc::vec;
use core::fmt;
use core::ptr::NonNull;

/// Where a buffer's bytes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryKind {
    /// Heap memory owned by the pool.
    Host,
    /// A memfd: plain shared memory, passed between processes by descriptor.
    Memfd,
    /// A dma-buf: memory a device reads or writes. CPU access is bracketed with sync calls.
    Dmabuf,
    /// GPU memory, not mapped for the CPU.
    Gpu,
}

/// What a CPU access does, for the sync brackets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CpuAccess {
    Read,
    Write,
}

/// One buffer's memory, given to a [`crate::FramePool`] once.
///
/// # Safety
///
/// [`BufferMemory::cpu_ptr`], when `Some`, is the start of [`BufferMemory::len`] bytes that stay
/// valid and at that address until the value is dropped, and that nothing else reads or writes
/// while the pool holds the value, except a device between sync brackets. When
/// [`BufferMemory::cpu_writable`] is `true` those bytes may be written through the pointer.
pub unsafe trait BufferMemory: Send + Sync {
    fn kind(&self) -> MemoryKind;

    /// Bytes in the buffer.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes as the CPU sees them. `None` when they are not mapped (GPU memory, a dma-buf not
    /// mapped here): such a frame's planes can be handed on but not read.
    fn cpu_ptr(&self) -> Option<NonNull<u8>>;

    /// Whether the CPU may write the bytes. Default `false`.
    fn cpu_writable(&self) -> bool {
        false
    }

    /// The start of a CPU access (dma-buf: `DMA_BUF_IOCTL_SYNC` START). Default nothing.
    fn begin_cpu(&self, _access: CpuAccess) {}

    /// The end of a CPU access (dma-buf: `DMA_BUF_IOCTL_SYNC` END). Default nothing.
    fn end_cpu(&self, _access: CpuAccess) {}

    /// The descriptor holding the bytes, for handing the buffer to another process or device.
    /// Default `None`.
    #[cfg(all(feature = "std", unix))]
    fn fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }
}

impl fmt::Debug for dyn BufferMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferMemory")
            .field("kind", &self.kind())
            .field("len", &self.len())
            .field("mapped", &self.cpu_ptr().is_some())
            .field("writable", &self.cpu_writable())
            .finish()
    }
}

/// Heap memory, allocated once when the buffer is made.
///
/// Allocated zeroed (the allocator hands out fresh pages for large buffers, so this costs no
/// writes), once, at registration. Frames written into it later reuse its bytes as they are.
pub struct HostMemory {
    ptr: NonNull<u8>,
    len: usize,
}

impl HostMemory {
    pub fn new(len: usize) -> Self {
        let boxed: Box<[u8]> = vec![0u8; len].into_boxed_slice();
        let ptr = NonNull::from(Box::leak(boxed)).cast::<u8>();
        Self { ptr, len }
    }
}

impl fmt::Debug for HostMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostMemory")
            .field("len", &self.len)
            .finish()
    }
}

impl Drop for HostMemory {
    fn drop(&mut self) {
        let slice = core::ptr::slice_from_raw_parts_mut(self.ptr.as_ptr(), self.len);
        // SAFETY: `ptr` and `len` came from `Box::leak` of a `Box<[u8]>` of `len` bytes in `new`,
        // and this is the only place it is given back.
        drop(unsafe { Box::from_raw(slice) });
    }
}

// SAFETY: the memory is a heap allocation owned by this value and not tied to a thread.
unsafe impl Send for HostMemory {}
// SAFETY: `&HostMemory` gives only the pointer; reads and writes through it are ordered by the
// pool's ownership rules (one writer, while it is the sole owner).
unsafe impl Sync for HostMemory {}

// SAFETY: `ptr` is the start of `len` bytes owned by this value until it drops, written only
// through the pool.
unsafe impl BufferMemory for HostMemory {
    fn kind(&self) -> MemoryKind {
        MemoryKind::Host
    }

    fn len(&self) -> usize {
        self.len
    }

    fn cpu_ptr(&self) -> Option<NonNull<u8>> {
        Some(self.ptr)
    }

    fn cpu_writable(&self) -> bool {
        true
    }
}
