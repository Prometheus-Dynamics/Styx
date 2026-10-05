//! Cached dma-heap capture buffers imported into libcamera.
//!
//! On Raspberry Pi, buffers from libcamera's `FrameBufferAllocator` are `videobuf2_dma_contig`
//! exports that the CPU maps uncached: scalar per-pixel reads of a 1280x800 Y plane take ~10 ms
//! on a CM5, versus ~0.7 ms for the same bytes in cached memory. Importing dma-heap buffers keeps
//! the ISP writing zero-copy while CPU reads run at normal memory speed. Reads are bracketed with
//! `DMA_BUF_IOCTL_SYNC` in [`super::backing::LibcameraBacking`].

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use libcamera::framebuffer::{AsFrameBuffer, FrameBufferPlane, OwnedFrameBuffer};
use libcamera::framebuffer_allocator::{FrameBuffer, FrameBufferAllocator};
use libcamera::request::Request;
use libcamera::stream::Stream;
use smallvec::SmallVec;

use crate::capture_api::LibcameraBufferMemory;

/// Cached heaps in preference order: Raspberry Pi OS's cached CMA heap, the generic CMA heap,
/// then the page-allocator heap (usable when the device sits behind an IOMMU).
const CACHED_HEAPS: &[&str] = &[
    "/dev/dma_heap/vidbuf_cached",
    "/dev/dma_heap/linux,cma",
    "/dev/dma_heap/system",
];

/// A capture buffer attached to a libcamera request.
pub(super) enum CaptureBuffer {
    Allocated(FrameBuffer),
    Heap(OwnedFrameBuffer),
}

impl CaptureBuffer {
    pub(super) fn as_framebuffer(&self) -> &dyn AsFrameBuffer {
        match self {
            Self::Allocated(buffer) => buffer,
            Self::Heap(buffer) => buffer,
        }
    }

    pub(super) fn add_to(self, request: &mut Request, stream: &Stream) -> io::Result<()> {
        match self {
            Self::Allocated(buffer) => request.add_buffer(stream, buffer),
            Self::Heap(buffer) => request.add_buffer(stream, buffer),
        }
    }
}

/// Look up the buffer attached to `stream`, whichever allocation path produced it.
pub(super) fn request_buffer<'a>(
    request: &'a Request,
    stream: &Stream,
) -> Option<&'a dyn AsFrameBuffer> {
    if let Some(buffer) = request.buffer::<FrameBuffer>(stream) {
        return Some(buffer);
    }
    request
        .buffer::<OwnedFrameBuffer>(stream)
        .map(|buffer| buffer as &dyn AsFrameBuffer)
}

/// Buffers for one stream plus the memory they came from (for logs and metrics).
pub(super) struct StreamBuffers {
    pub buffers: Vec<CaptureBuffer>,
    pub memory: String,
}

/// Resolve which dma-heap to use, if any.
pub(super) fn select_heap(memory: LibcameraBufferMemory, raspberry_pi: bool) -> Option<PathBuf> {
    let memory = buffer_memory_env_override().unwrap_or(memory);
    let wants_heap = match memory {
        LibcameraBufferMemory::LibcameraAllocator => false,
        LibcameraBufferMemory::DmaHeap => true,
        LibcameraBufferMemory::Auto => raspberry_pi,
    };
    if !wants_heap {
        return None;
    }
    CACHED_HEAPS
        .iter()
        .map(Path::new)
        .find(|path| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .is_ok()
        })
        .map(Path::to_path_buf)
}

fn buffer_memory_env_override() -> Option<LibcameraBufferMemory> {
    let value = std::env::var("STYX_LIBCAMERA_BUFFER_MEMORY").ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(LibcameraBufferMemory::Auto),
        "libcamera" | "allocator" | "libcamera-allocator" => {
            Some(LibcameraBufferMemory::LibcameraAllocator)
        }
        "dma-heap" | "dmaheap" | "heap" => Some(LibcameraBufferMemory::DmaHeap),
        _ => None,
    }
}

/// Allocate capture buffers for `stream`.
///
/// libcamera's allocator always runs first so the pipeline handler decides the exact plane
/// layout (fd grouping, offsets, lengths). With a heap selected, the same layout is then
/// recreated in cached dma-heap memory and the allocator's buffers are freed. Any heap failure
/// keeps the allocator's buffers, so capture never fails because of this optimisation.
pub(super) fn allocate_stream_buffers(
    alloc: &mut FrameBufferAllocator,
    stream: &Stream,
    heap: Option<&Path>,
) -> io::Result<StreamBuffers> {
    let allocated = alloc.alloc(stream)?;
    let Some(heap) = heap else {
        return Ok(allocator_buffers(allocated));
    };
    let Some(template) = allocated.first().map(BufferTemplate::of) else {
        return Ok(allocator_buffers(allocated));
    };
    match allocate_heap_buffers(heap, &template, allocated.len()) {
        Ok(buffers) => {
            drop(allocated);
            if let Err(err) = alloc.free(stream) {
                crate::trace::debug!(backend = "libcamera", error = %err, "libcamera allocator free failed");
            }
            Ok(StreamBuffers {
                buffers,
                memory: format!("dma-heap:{}", heap.display()),
            })
        }
        Err(err) => {
            crate::trace::warn!(
                backend = "libcamera",
                heap = %heap.display(),
                error = %err,
                "dma-heap capture buffers unavailable; using uncached libcamera buffers"
            );
            Ok(allocator_buffers(allocated))
        }
    }
}

fn allocator_buffers(allocated: Vec<FrameBuffer>) -> StreamBuffers {
    StreamBuffers {
        buffers: allocated
            .into_iter()
            .map(CaptureBuffer::Allocated)
            .collect(),
        memory: "libcamera-allocator".into(),
    }
}

/// Plane layout of one allocator buffer: which backing allocation each plane lives in.
struct BufferTemplate {
    allocation_sizes: SmallVec<[usize; 3]>,
    planes: SmallVec<[(usize, u32, u32); 3]>,
}

impl BufferTemplate {
    fn of(buffer: &FrameBuffer) -> Self {
        let mut fds = SmallVec::<[RawFd; 3]>::new();
        let mut allocation_sizes = SmallVec::<[usize; 3]>::new();
        let mut planes = SmallVec::<[(usize, u32, u32); 3]>::new();
        let buffer_planes = buffer.planes();
        for index in 0..buffer_planes.len() {
            let Some(plane) = buffer_planes.get(index) else {
                break;
            };
            let fd = plane.fd();
            let offset = plane.offset().unwrap_or(0);
            let len = plane.len();
            let index = match fds.iter().position(|known| *known == fd) {
                Some(index) => index,
                None => {
                    fds.push(fd);
                    allocation_sizes.push(fd_size(fd).unwrap_or(0));
                    fds.len() - 1
                }
            };
            let end = offset.saturating_add(len);
            allocation_sizes[index] = allocation_sizes[index].max(end);
            planes.push((index, offset as u32, len as u32));
        }
        Self {
            allocation_sizes,
            planes,
        }
    }
}

/// Identity of an open file (device, inode), or `None` if `fd` is not open.
type FileId = (u64, u64);

fn file_id(fd: RawFd) -> Option<FileId> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `st` is valid writable storage; fstat reports EBADF for closed fds.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: fstat succeeded and initialised `st`.
    let st = unsafe { st.assume_init() };
    // `dev_t`/`ino_t` widths differ between targets.
    #[allow(clippy::unnecessary_cast)]
    Some((st.st_dev as u64, st.st_ino as u64))
}

fn fd_size(fd: RawFd) -> Option<usize> {
    // SAFETY: lseek on a borrowed fd only moves its file offset, which dma-bufs ignore.
    let end = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    (end > 0).then_some(end as usize)
}

fn allocate_heap_buffers(
    heap_path: &Path,
    template: &BufferTemplate,
    count: usize,
) -> io::Result<Vec<CaptureBuffer>> {
    if template.planes.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no planes"));
    }
    let heap = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(heap_path)?;
    let mut buffers = Vec::with_capacity(count);
    for _ in 0..count {
        let allocations = template
            .allocation_sizes
            .iter()
            .map(|size| heap_alloc(heap.as_raw_fd(), *size))
            .collect::<io::Result<SmallVec<[OwnedFd; 3]>>>()?;
        let mut planes = Vec::with_capacity(template.planes.len());
        for (index, offset, length) in &template.planes {
            planes.push(FrameBufferPlane {
                fd: allocations[*index].try_clone()?,
                offset: *offset,
                length: *length,
            });
        }
        let passed: SmallVec<[(RawFd, Option<FileId>); 3]> = planes
            .iter()
            .map(|p| (p.fd.as_raw_fd(), file_id(p.fd.as_raw_fd())))
            .collect();
        let buffer = OwnedFrameBuffer::new(planes, None)?;
        // libcamera duplicates plane fds. libcamera-rs 0.7.0 releases ours with `into_raw_fd`
        // and never closes them (fixed upstream by closing them itself). Close any that are
        // still open and still refer to the same dma-buf, which is correct for both versions.
        for (fd, id) in passed {
            if id.is_some() && file_id(fd) == id {
                // SAFETY: the fd is open, refers to the dma-buf we allocated above, and nothing
                // else owns it: libcamera holds its own duplicate.
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
        buffers.push(CaptureBuffer::Heap(buffer));
    }
    Ok(buffers)
}

#[repr(C)]
struct DmaHeapAllocationData {
    len: u64,
    fd: u32,
    fd_flags: u32,
    heap_flags: u64,
}

// _IOWR('H', 0x0, struct dma_heap_allocation_data)
const DMA_HEAP_IOCTL_ALLOC: libc::c_ulong = 0xc018_4800;

fn heap_alloc(heap: RawFd, len: usize) -> io::Result<OwnedFd> {
    let page = 4096usize;
    let mut data = DmaHeapAllocationData {
        len: len.div_ceil(page).max(1).saturating_mul(page) as u64,
        fd: 0,
        fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
        heap_flags: 0,
    };
    // SAFETY: `data` is a valid, writable `struct dma_heap_allocation_data`; the kernel fills in
    // `fd` on success.
    let ret = unsafe { libc::ioctl(heap, DMA_HEAP_IOCTL_ALLOC as _, &mut data) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success the kernel returned a fresh dma-buf fd that we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(data.fd as RawFd) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dma_heap_ioctl_number_matches_linux_uapi() {
        // _IOC(_IOC_READ|_IOC_WRITE=3, 'H'=0x48, nr=0, size=24)
        let expected: libc::c_ulong = (3 << 30) | (24 << 16) | (0x48 << 8);
        assert_eq!(DMA_HEAP_IOCTL_ALLOC, expected);
        assert_eq!(std::mem::size_of::<DmaHeapAllocationData>(), 24);
    }

    #[test]
    fn allocator_is_kept_when_configured() {
        assert_eq!(
            select_heap(LibcameraBufferMemory::LibcameraAllocator, true),
            None
        );
        assert_eq!(select_heap(LibcameraBufferMemory::Auto, false), None);
    }
}
