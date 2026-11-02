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

//! The ioctls are Lemnos's (`lemnos_linux::dma`, `lemnos_linux_sys::dma_buf`); Styx keeps
//! the heap sizing ([`TAIL_SLACK`]), the sync counters ([`sync_stats`]) and the memfd stand-in.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use lemnos_linux::dma as lemnos;
use lemnos_linux_sys::dma_buf as sys;

use crate::{Error, Mapping, Result};

/// Where dma-heaps live.
pub const HEAP_DIR: &str = lemnos::HEAP_DIR;

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
            Access::Read => sys::SYNC_READ,
            Access::Write => sys::SYNC_WRITE,
            Access::ReadWrite => sys::SYNC_READ | sys::SYNC_WRITE,
        }
    }
}

fn sys_err(call: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Sys { call, source }
}

static SYNCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SYNC_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Brackets CPU access to any dma-buf (`DMA_BUF_IOCTL_SYNC`): call with `start = true` before
/// touching a mapping and `start = false` after. Counted with its time ([`sync_stats`]).
pub fn sync(dmabuf: BorrowedFd<'_>, access: Access, start: bool) -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let flags = access.bits()
        | if start {
            sys::SYNC_START
        } else {
            sys::SYNC_END
        };
    let at = std::time::Instant::now();
    let r = sys::sync(dmabuf, flags);
    SYNCS.fetch_add(1, Relaxed);
    SYNC_NS.fetch_add(at.elapsed().as_nanos() as u64, Relaxed);
    r.map_err(sys_err("DMA_BUF_IOCTL_SYNC"))
}

/// `DMA_BUF_IOCTL_SYNC` calls [`sync`] made in this process, and the nanoseconds they took.
pub fn sync_stats() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (SYNCS.load(Relaxed), SYNC_NS.load(Relaxed))
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
    /// close-on-exec: [`padded_len`] (its [`DmaBuf::len`]).
    pub fn allocate(&self, len: usize) -> Result<DmaBuf> {
        if len == 0 {
            return Err(Error::Invalid("cannot allocate an empty dma-buf".into()));
        }
        let len = padded_len(len);
        let buf = self
            .0
            .allocate(len)
            .map_err(sys_err("DMA_HEAP_IOCTL_ALLOC"))?;
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
/// the frame.
pub const TAIL_SLACK: usize = 256;

/// The size Styx allocates for `len` bytes: [`TAIL_SLACK`] more, rounded up to 4 KiB pages
/// (what the heaps allocate anyway, so the slack costs nothing for most sizes).
pub const fn padded_len(len: usize) -> usize {
    (len + TAIL_SLACK).next_multiple_of(4096)
}

/// Lists the heaps in `/dev/dma_heap`, sorted by name.
pub fn list_heaps() -> Vec<String> {
    lemnos::list_heaps().unwrap_or_default()
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

    /// A `memfd` of `len` bytes standing in for a dma-buf: shareable and mappable like one, but
    /// plain memory (CPU-access syncs on it fail with `ENOTTY`). For tests and software
    /// producers.
    pub fn memfd(name: &str, len: usize) -> Result<Self> {
        if len == 0 {
            return Err(Error::Invalid("cannot allocate an empty memfd".into()));
        }
        let name = std::ffi::CString::new(name)
            .map_err(|_| Error::Invalid("memfd name contains a NUL byte".into()))?;
        // SAFETY: `name` is a valid NUL-terminated string for the call.
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(Error::sys("memfd_create"));
        }
        // SAFETY: memfd_create returned a new descriptor that we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        std::fs::File::from(fd.try_clone().map_err(|e| Error::Sys {
            call: "dup",
            source: e,
        })?)
        .set_len(len as u64)
        .map_err(|e| Error::Sys {
            call: "ftruncate",
            source: e,
        })?;
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

    #[test]
    fn a_memfd_maps_like_a_dma_buf_and_outlives_its_descriptor() {
        let buf = DmaBuf::memfd("styx-test", 8192).unwrap();
        assert_eq!(buf.len(), 8192);
        let mut a = buf.map().unwrap();
        a.as_mut_slice()[100] = 7;
        let b = DmaBuf::from_fd(buf.into_fd(), 8192).map().unwrap();
        // The descriptor is closed; both mappings still share the memory.
        assert_eq!(b.as_slice()[100], 7);
        assert!(DmaBuf::memfd("empty", 0).is_err());
    }

    #[test]
    fn heap_buffers_have_tail_slack_in_whole_pages() {
        assert_eq!(padded_len(1_228_800), 1_232_896); // 1280x800 GREY: v3dv wants 1_229_056
        assert!(padded_len(1_228_800) >= 1_228_800 + TAIL_SLACK);
        assert_eq!(padded_len(4096 - TAIL_SLACK), 4096);
        assert_eq!(padded_len(4096), 8192);
    }

    #[test]
    fn dmabuf_size_reports_what_the_kernel_allocated() {
        let buf = DmaBuf::memfd("styx-test", 12_345).unwrap();
        assert_eq!(dmabuf_size(buf.as_fd()).unwrap(), 12_345);
        // Twice: the offset went back to the start.
        assert_eq!(dmabuf_size(buf.as_fd()).unwrap(), 12_345);
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
