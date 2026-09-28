//! dma-heaps: allocating dma-bufs for zero-copy buffers, and CPU-access syncs on dma-bufs.
//!
//! ```no_run
//! use styx_kernel::dma_heap::{Access, DmaHeap};
//!
//! let heap = DmaHeap::open("system")?;
//! let buf = heap.allocate(1 << 20)?;
//! let mut map = buf.map()?;
//! buf.begin_cpu_access(Access::Write)?;
//! map.as_mut_slice().fill(0);
//! buf.end_cpu_access(Access::Write)?;
//! # Ok::<(), styx_kernel::Error>(())
//! ```

#![allow(non_camel_case_types)]

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use crate::ioctl::{self, ioctls, iow, iowr};
use crate::{Error, Mapping, Result};

/// Where dma-heaps live.
pub const HEAP_DIR: &str = "/dev/dma_heap";

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct dma_heap_allocation_data {
    len: u64,
    fd: u32,
    fd_flags: u32,
    heap_flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct dma_buf_sync {
    flags: u64,
}

ioctls! {
    DMA_HEAP_IOCTL_ALLOC = iowr::<dma_heap_allocation_data>(b'H', 0);
    DMA_BUF_IOCTL_SYNC = iow::<dma_buf_sync>(b'b', 0);
}

const _: () = assert!(size_of::<dma_heap_allocation_data>() == 24);
const _: () = assert!(size_of::<dma_buf_sync>() == 8);

const SYNC_READ: u64 = 1 << 0;
const SYNC_WRITE: u64 = 1 << 1;
const SYNC_START: u64 = 0;
const SYNC_END: u64 = 1 << 2;

/// The direction of CPU access to a dma-buf.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// The CPU reads (e.g. a captured frame).
    Read,
    /// The CPU writes (e.g. ISP parameters).
    Write,
    /// Both.
    ReadWrite,
}

impl Access {
    fn bits(self) -> u64 {
        match self {
            Access::Read => SYNC_READ,
            Access::Write => SYNC_WRITE,
            Access::ReadWrite => SYNC_READ | SYNC_WRITE,
        }
    }
}

/// Brackets CPU access to any dma-buf (`DMA_BUF_IOCTL_SYNC`): call with `start = true` before
/// touching a mapping and `start = false` after.
pub fn sync(dmabuf: BorrowedFd<'_>, access: Access, start: bool) -> Result<()> {
    let mut raw = dma_buf_sync {
        flags: access.bits() | if start { SYNC_START } else { SYNC_END },
    };
    // SAFETY: DMA_BUF_IOCTL_SYNC takes a `dma_buf_sync`.
    unsafe { ioctl::ioctl(dmabuf, DMA_BUF_IOCTL_SYNC, &mut raw)? };
    Ok(())
}

/// An open dma-heap (`/dev/dma_heap/<name>`).
#[derive(Debug)]
pub struct DmaHeap {
    fd: OwnedFd,
    path: PathBuf,
}

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
        let path = path.as_ref();
        Ok(Self {
            fd: ioctl::open_path(path, libc::O_RDONLY, false)?,
            path: path.to_owned(),
        })
    }

    /// The heap's name (the file name of its node).
    pub fn name(&self) -> &str {
        self.path.file_name().and_then(|n| n.to_str()).unwrap_or("")
    }

    /// Allocates a dma-buf of `len` bytes (`DMA_HEAP_IOCTL_ALLOC`), read-write and
    /// close-on-exec.
    pub fn allocate(&self, len: usize) -> Result<DmaBuf> {
        if len == 0 {
            return Err(Error::Invalid("cannot allocate an empty dma-buf".into()));
        }
        let mut raw = dma_heap_allocation_data {
            len: len as u64,
            fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
            ..Default::default()
        };
        // SAFETY: DMA_HEAP_IOCTL_ALLOC takes a `dma_heap_allocation_data`.
        unsafe { ioctl::ioctl(self.as_fd(), DMA_HEAP_IOCTL_ALLOC, &mut raw)? };
        // SAFETY: the kernel returned a new dma-buf descriptor that we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw.fd as RawFd) };
        Ok(DmaBuf { fd, len })
    }
}

impl AsFd for DmaHeap {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Lists the heaps in `/dev/dma_heap`, sorted by name.
pub fn list_heaps() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(HEAP_DIR)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// A dma-buf allocated from a heap.
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
        Mapping::new(self.fd.as_fd(), self.len, 0, true)
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
mod tests {
    use super::*;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn ioctl_numbers_match_the_headers() {
        assert_eq!(DMA_HEAP_IOCTL_ALLOC.nr, 0xc018_4800);
        assert_eq!(DMA_BUF_IOCTL_SYNC.nr, 0x4008_6200);
    }

    #[test]
    fn rejects_bad_names() {
        assert!(DmaHeap::open("../etc").is_err());
        assert!(DmaHeap::open("").is_err());
    }

    #[test]
    fn allocates_and_syncs_when_a_heap_exists() {
        let Some(name) = list_heaps()
            .into_iter()
            .find(|n| n == "system")
            .or_else(|| list_heaps().pop())
        else {
            eprintln!("no dma-heaps; skipping");
            return;
        };
        let heap = match DmaHeap::open(&name) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("cannot open heap {name}: {e}; skipping");
                return;
            }
        };
        let buf = heap.allocate(4096).expect("allocate");
        let mut map = buf.map().expect("map");
        buf.begin_cpu_access(Access::Write).expect("sync start");
        map.as_mut_slice().fill(0xa5);
        buf.end_cpu_access(Access::Write).expect("sync end");
        assert!(map.as_slice().iter().all(|&b| b == 0xa5));
    }
}
