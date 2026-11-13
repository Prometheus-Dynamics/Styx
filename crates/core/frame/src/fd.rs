//! memfd and dma-buf memory (Linux), through Lemnos's library crate.

use core::fmt;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use lemnos_linux_sys::dma_buf;
use lemnos_linux_sys::mmap::Mapping;

use crate::memory::{BufferMemory, CpuAccess, MemoryKind};

/// A buffer held by a descriptor: a memfd or a dma-buf, mapped for the CPU or not.
///
/// A dma-buf's CPU access is bracketed with `DMA_BUF_IOCTL_SYNC` (Lemnos `dma_buf::sync`). A
/// failed sync is counted ([`FdMemory::sync_errors`]); the access goes ahead, as the kernel's own
/// users do when the exporter has no cache to maintain.
pub struct FdMemory {
    fd: OwnedFd,
    mapping: Option<Mapping>,
    kind: MemoryKind,
    len: usize,
    writable: bool,
    sync_errors: AtomicUsize,
}

impl FdMemory {
    /// A memfd of `len` bytes, mapped (read-write when `writable`).
    pub fn memfd(fd: OwnedFd, len: usize, writable: bool) -> io::Result<Self> {
        let mapping = Mapping::new(fd.as_fd(), len, 0, writable)?;
        Ok(Self::with(
            fd,
            Some(mapping),
            MemoryKind::Memfd,
            len,
            writable,
        ))
    }

    /// A dma-buf of `len` bytes, mapped (read-write when `writable`).
    pub fn dmabuf(fd: OwnedFd, len: usize, writable: bool) -> io::Result<Self> {
        let mapping = Mapping::new(fd.as_fd(), len, 0, writable)?;
        Ok(Self::with(
            fd,
            Some(mapping),
            MemoryKind::Dmabuf,
            len,
            writable,
        ))
    }

    /// A dma-buf of `len` bytes the CPU does not map: handed on by descriptor only.
    pub fn dmabuf_unmapped(fd: OwnedFd, len: usize) -> Self {
        Self::with(fd, None, MemoryKind::Dmabuf, len, false)
    }

    fn with(
        fd: OwnedFd,
        mapping: Option<Mapping>,
        kind: MemoryKind,
        len: usize,
        writable: bool,
    ) -> Self {
        Self {
            fd,
            mapping,
            kind,
            len,
            writable,
            sync_errors: AtomicUsize::new(0),
        }
    }

    /// Sync calls the kernel refused.
    pub fn sync_errors(&self) -> usize {
        self.sync_errors.load(Ordering::Relaxed)
    }

    fn sync(&self, access: CpuAccess, end: bool) {
        if self.kind != MemoryKind::Dmabuf || self.mapping.is_none() {
            return;
        }
        let direction = match access {
            CpuAccess::Read => dma_buf::SYNC_READ,
            CpuAccess::Write => dma_buf::SYNC_WRITE,
        };
        let edge = if end {
            dma_buf::SYNC_END
        } else {
            dma_buf::SYNC_START
        };
        if dma_buf::sync(self.fd.as_fd(), edge | direction).is_err() {
            self.sync_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl fmt::Debug for FdMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FdMemory")
            .field("kind", &self.kind)
            .field("len", &self.len)
            .field("mapped", &self.mapping.is_some())
            .finish()
    }
}

// SAFETY: when mapped, `cpu_ptr` is the start of the `len`-byte shared mapping this value owns
// until it drops (Lemnos `Mapping` unmaps on drop), writable only when mapped read-write. Other
// processes or devices holding the descriptor write it only under the frame's hold rules and the
// sync brackets.
unsafe impl BufferMemory for FdMemory {
    fn kind(&self) -> MemoryKind {
        self.kind
    }

    fn len(&self) -> usize {
        self.len
    }

    fn cpu_ptr(&self) -> Option<NonNull<u8>> {
        let mapping = self.mapping.as_ref()?;
        NonNull::new(mapping.as_ptr().cast_mut())
    }

    fn cpu_writable(&self) -> bool {
        self.mapping.is_some() && self.writable
    }

    fn begin_cpu(&self, access: CpuAccess) {
        self.sync(access, false);
    }

    fn end_cpu(&self, access: CpuAccess) {
        self.sync(access, true);
    }

    fn fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.fd.as_fd())
    }
}
