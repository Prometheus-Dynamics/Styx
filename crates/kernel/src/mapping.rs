//! Memory mappings of device buffers (V4L2 MMAP planes, dma-bufs).

use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr::NonNull;

use crate::{Error, Result};

/// A shared, read-write memory mapping of a device buffer, unmapped on drop.
///
/// The device may write to the memory while a buffer is queued; read it only while the buffer
/// is owned by userspace (dequeued, or bracketed by dma-buf CPU-access syncs).
#[derive(Debug)]
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain shared memory owned by this value; it has no thread affinity.
unsafe impl Send for Mapping {}
// SAFETY: shared access only hands out `&[u8]`; mutation needs `&mut self`.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Maps `len` bytes of `fd` at `offset` (`PROT_READ | PROT_WRITE`, `MAP_SHARED`), or
    /// read-only when `writable` is false.
    pub(crate) fn new(fd: BorrowedFd<'_>, len: usize, offset: i64, writable: bool) -> Result<Self> {
        if len == 0 {
            return Err(Error::Invalid("cannot map a zero-length buffer".into()));
        }
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        // SAFETY: a fresh shared mapping at an address chosen by the kernel; no existing memory
        // is affected. The fd stays valid for the call.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(Error::sys("mmap"));
        }
        let ptr = NonNull::new(ptr.cast::<u8>())
            .ok_or_else(|| Error::Invalid("mmap returned null".into()))?;
        Ok(Self { ptr, len })
    }

    /// The mapped length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True for an empty mapping (never happens; present for API symmetry).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The start address.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// The mapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is a live mapping of `len` bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The mapped bytes, writable (for output buffers). Writing to a read-only mapping faults.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above; `&mut self` gives exclusive access from this process's side.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` describe a mapping created by `mmap` and not yet unmapped.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}
