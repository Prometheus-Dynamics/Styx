//! Styx's dma-bufs on Lemnos's (`lemnos_linux::dma`): every heap allocation carries
//! [`TAIL_SLACK`] for Vulkan import (Lemnos's `allocate_importable`), a memfd stands in for a dma-buf in tests and software
//! producers, and errors are [`lemnos_v4l2::Error`]s like the rest of the device code.
//!
//! ```no_run
//! use styx_native::dma::{Access, DmaHeap};
//!
//! let heap = DmaHeap::open("system")?;
//! let buf = heap.allocate(1 << 20)?;
//! let mut map = buf.map()?;
//! buf.begin_cpu_access(Access::Write)?;
//! map.as_mut_slice().fill(0);
//! buf.end_cpu_access(Access::Write)?;
//! # Ok::<(), lemnos_v4l2::Error>(())
//! ```
//!
//! The syncs are counted by Lemnos ([`sync_stats`], every `DMA_BUF_IOCTL_SYNC` of the process).

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;

use lemnos_linux::dma as lemnos;
pub use lemnos_linux::dma::{Access, HEAP_DIR, sync_stats};
use lemnos_linux_sys::dma_buf as sys;
use lemnos_v4l2::{Error, Mapping, Result};

fn sys_err(call: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Sys { call, source }
}

/// Brackets CPU access to any dma-buf (`DMA_BUF_IOCTL_SYNC`): call with `start = true` before
/// touching a mapping and `start = false` after. Counted with its time ([`sync_stats`]).
pub fn sync(dmabuf: BorrowedFd<'_>, access: Access, start: bool) -> Result<()> {
    let phase = if start {
        lemnos::Phase::Begin
    } else {
        lemnos::Phase::End
    };
    lemnos::sync(dmabuf, phase, access).map_err(sys_err("DMA_BUF_IOCTL_SYNC"))
}

/// Which dma-buf `fd` is (its device and inode), to know it again under another descriptor.
pub fn dmabuf_id(fd: BorrowedFd<'_>) -> Option<(u64, u64)> {
    sys::identity(fd).ok()
}

/// The size of a dma-buf (or any seekable file, e.g. a memfd) in bytes, as the kernel has it.
/// The descriptor's file offset is left where it was.
pub fn dmabuf_size(dmabuf: BorrowedFd<'_>) -> Result<u64> {
    sys::size(dmabuf).map_err(sys_err("lseek"))
}

/// An open dma-heap (`/dev/dma_heap/<name>`).
#[derive(Debug)]
pub struct DmaHeap(lemnos::DmaHeap);

impl DmaHeap {
    /// Opens a heap by name, e.g. `system`, `linux,cma`, `vidbuf_cached`.
    pub fn open(name: &str) -> Result<Self> {
        if name.is_empty() || name.contains('/') {
            return Err(Error::Invalid(format!("invalid dma-heap name {name:?}")));
        }
        Self::open_path(Path::new(HEAP_DIR).join(name))
    }

    /// Opens a heap by path.
    pub fn open_path(path: impl AsRef<Path>) -> Result<Self> {
        lemnos::DmaHeap::open_path(path)
            .map(Self)
            .map_err(sys_err("open"))
    }

    /// The heap's name (the file name of its node).
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// Allocates a dma-buf of at least `len` bytes (`DMA_HEAP_IOCTL_ALLOC`), read-write and
    /// close-on-exec, with [`TAIL_SLACK`] more rounded up to whole pages of this system
    /// (`lemnos_linux::dma::DmaHeap::allocate_importable`; its [`DmaBuf::len`]).
    pub fn allocate(&self, len: usize) -> Result<DmaBuf> {
        if len == 0 {
            return Err(Error::Invalid("cannot allocate an empty dma-buf".into()));
        }
        let buf = self
            .0
            .allocate_importable(len)
            .map_err(sys_err("DMA_HEAP_IOCTL_ALLOC"))?;
        let len = buf.len();
        Ok(DmaBuf {
            fd: buf.into_fd(),
            len,
        })
    }
}

impl AsFd for DmaHeap {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Bytes every dma-buf Styx allocates has beyond its contents: Vulkan drivers (Mesa's v3dv)
/// import a linear dma-buf as a storage buffer only if it is at least this much larger than
/// the frame. Lemnos's `IMPORT_SLACK`.
pub use lemnos::IMPORT_SLACK as TAIL_SLACK;

/// Lists the heaps in `/dev/dma_heap`, sorted by name.
pub fn list_heaps() -> Vec<String> {
    lemnos::list_heaps().unwrap_or_default()
}

/// A dma-buf allocated from a heap, or a memfd standing in for one.
#[derive(Debug)]
pub struct DmaBuf {
    fd: OwnedFd,
    len: usize,
}

impl DmaBuf {
    /// Wraps an existing dma-buf descriptor (e.g. from `VIDIOC_EXPBUF`) of `len` bytes.
    pub fn from_fd(fd: OwnedFd, len: usize) -> Self {
        Self { fd, len }
    }

    /// A `memfd` of `len` bytes standing in for a dma-buf: shareable and mappable like one, but
    /// plain memory (CPU-access syncs on it fail with `ENOTTY`). For tests and software
    /// producers.
    pub fn memfd(name: &str, len: usize) -> Result<Self> {
        if len == 0 {
            return Err(Error::Invalid("cannot allocate an empty memfd".into()));
        }
        let name = std::ffi::CString::new(name)
            .map_err(|_| Error::Invalid("memfd name contains a NUL byte".into()))?;
        let fd = sys::memfd(&name).map_err(sys_err("memfd_create"))?;
        std::fs::File::from(fd.try_clone().map_err(sys_err("dup"))?)
            .set_len(len as u64)
            .map_err(sys_err("ftruncate"))?;
        Ok(Self { fd, len })
    }

    /// Size in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True for an empty buffer (never, for allocated buffers).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Maps the whole buffer read-write.
    pub fn map(&self) -> Result<Mapping> {
        Mapping::new(self.fd.as_fd(), self.len, 0, true).map_err(sys_err("mmap"))
    }

    /// Starts CPU access (`DMA_BUF_IOCTL_SYNC` with `SYNC_START`).
    pub fn begin_cpu_access(&self, access: Access) -> Result<()> {
        sync(self.fd.as_fd(), access, true)
    }

    /// Ends CPU access (`DMA_BUF_IOCTL_SYNC` with `SYNC_END`).
    pub fn end_cpu_access(&self, access: Access) -> Result<()> {
        sync(self.fd.as_fd(), access, false)
    }

    /// Gives up ownership of the descriptor.
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

impl AsFd for DmaBuf {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for DmaBuf {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
#[path = "dma_tests.rs"]
mod tests;
