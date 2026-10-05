//! CPU cache maintenance for dma-buf mappings (`DMA_BUF_IOCTL_SYNC`).
//!
//! Devices such as ISPs and scalers on ARM SoCs write dma-bufs without snooping CPU caches. When
//! the exporter maps those buffers cached (dma-heap buffers do), the CPU must bracket its reads
//! with a sync START/END pair or it may read stale cache lines, and its writes with another so
//! devices read them (not what is left in the CPU's cache). Buffers mapped uncached make the
//! ioctl a cheap no-op, and non-dma-buf fds reject it with `ENOTTY`.

use std::io;
use std::os::fd::RawFd;

const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_WRITE: u64 = 1 << 1;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 1 << 2;
// _IOW('b', 0, struct dma_buf_sync { __u64 flags; })
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

fn dma_buf_sync(fd: RawFd, flags: u64) -> io::Result<()> {
    let arg = DmaBufSync { flags };
    loop {
        // SAFETY: `arg` is a valid `struct dma_buf_sync` for the duration of the call and the
        // kernel only reads it. An invalid fd is reported through the return value.
        let ret = unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &arg as *const DmaBufSync) };
        if ret == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Begin a CPU read of a dma-buf: invalidates stale CPU cache lines for the buffer.
pub fn dmabuf_begin_cpu_read(fd: RawFd) -> io::Result<()> {
    dma_buf_sync(fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)
}

/// End a CPU read started with [`dmabuf_begin_cpu_read`].
pub fn dmabuf_end_cpu_read(fd: RawFd) -> io::Result<()> {
    dma_buf_sync(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ)
}

/// Begin a CPU write of a dma-buf (reads included until the end: `DMA_BUF_SYNC_RW`):
/// invalidates stale CPU cache lines. A writable `MemoryRegion`'s
/// [`RegionHooks::begin_cpu_write`](crate::buffer::RegionHooks::begin_cpu_write) over a dma-buf
/// mapping calls it.
pub fn dmabuf_begin_cpu_write(fd: RawFd) -> io::Result<()> {
    dma_buf_sync(
        fd,
        DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE,
    )
}

/// End a CPU write started with [`dmabuf_begin_cpu_write`]: writes the CPU's cache lines back
/// so devices read what the CPU wrote
/// ([`RegionHooks::end_cpu_write`](crate::buffer::RegionHooks::end_cpu_write)).
pub fn dmabuf_end_cpu_write(fd: RawFd) -> io::Result<()> {
    dma_buf_sync(
        fd,
        DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_number_matches_linux_uapi() {
        // _IOC(_IOC_WRITE=1, 'b'=0x62, nr=0, size=8)
        let expected: libc::c_ulong = (1 << 30) | (8 << 16) | (0x62 << 8);
        assert_eq!(DMA_BUF_IOCTL_SYNC, expected);
    }

    #[test]
    fn non_dmabuf_fds_are_rejected_without_side_effects() {
        let file = std::fs::File::open("/proc/self/stat").expect("open");
        use std::os::fd::AsRawFd;
        assert!(dmabuf_begin_cpu_read(file.as_raw_fd()).is_err());
        assert!(dmabuf_begin_cpu_write(file.as_raw_fd()).is_err());
        assert!(dmabuf_end_cpu_write(file.as_raw_fd()).is_err());
    }
}
