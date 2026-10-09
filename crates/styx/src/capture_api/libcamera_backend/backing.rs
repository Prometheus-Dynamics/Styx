use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use libcamera::framebuffer::AsFrameBuffer;
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::os::fd::{FromRawFd, OwnedFd};
use styx_core::prelude::{
    CpuAccess, ExportedKind, ExternalBacking, FrameBackingExport, FrameExportError, FrameFdPlane,
    FrameResidency,
};

use crate::metrics::ExternalBackingTracker;

pub(super) fn wait_for_backings_to_drain(
    outstanding_backings: &AtomicUsize,
    timeout: Duration,
    poll: Duration,
) -> bool {
    let start = Instant::now();
    let poll = poll.max(Duration::from_millis(1));
    loop {
        let outstanding = outstanding_backings.load(Ordering::Acquire);
        if outstanding == 0 {
            crate::trace::debug!(
                backend = "libcamera",
                idle_drain_ms = start.elapsed().as_millis() as u64,
                "libcamera external backings drained"
            );
            return true;
        }
        if start.elapsed() >= timeout {
            crate::trace::debug!(
                backend = "libcamera",
                outstanding_backings = outstanding,
                idle_drain_ms = start.elapsed().as_millis() as u64,
                timeout_ms = timeout.as_millis() as u64,
                "libcamera external backing drain timed out"
            );
            return false;
        }
        thread::sleep(poll);
    }
}

fn system_page_size() -> usize {
    // SAFETY: `sysconf(_SC_PAGESIZE)` has no pointer arguments and is thread-safe.
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 { ps as usize } else { 4096 }
}

pub(super) fn infer_stride(bytes_used: usize, plane_len: usize, plane_height: usize) -> usize {
    if plane_height == 0 {
        return bytes_used.max(plane_len);
    }
    let by_used = if bytes_used > 0 {
        bytes_used
    } else {
        plane_len
    };
    let mut stride = by_used / plane_height;
    if stride == 0 {
        stride = 1;
    }
    let max_stride = plane_len / plane_height;
    if max_stride > 0 {
        stride = stride.min(max_stride);
    }
    stride
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct BackingPlaneView {
    pub fd: i32,
    pub offset: usize,
    pub len: usize,
}

#[derive(Clone, Copy)]
struct MappedPlaneRange {
    ptr: *mut core::ffi::c_void,
    len: usize,
    map_offset: usize,
}

struct LazyMappedBackingState {
    /// One mapping per dma-buf (the fd of its first plane): libcamera gives each plane its own
    /// duplicate of the buffer's fd, so planes are grouped by the file they refer to.
    mmaps: SmallVec<[(i32, MappedPlaneRange); 3]>,
    /// Plane `i`'s mapping: an index into `mmaps`.
    plane_maps: SmallVec<[u8; 3]>,
    mapped_bytes: usize,
}

// SAFETY: the mappings are read-only, created once, and unmapped only when the last reference
// drops; sharing the pointers between threads exposes immutable bytes only.
unsafe impl Send for LazyMappedBackingState {}
// SAFETY: see `Send`.
unsafe impl Sync for LazyMappedBackingState {}

impl LazyMappedBackingState {
    /// Invalidate stale CPU cache lines before a frame's first read. Required for cached
    /// dma-heap buffers written by the ISP; a cheap no-op for uncached allocator buffers. One
    /// sync per dma-buf, however many planes (and duplicated fds) it holds.
    fn begin_cpu_read(&self) {
        for (fd, _) in self.mmaps.iter().filter(|(_, r)| r.len > 0) {
            let _ = styx_core::prelude::dmabuf_begin_cpu_read(*fd);
        }
    }

    fn end_cpu_read(&self) {
        for (fd, _) in self.mmaps.iter().filter(|(_, r)| r.len > 0) {
            let _ = styx_core::prelude::dmabuf_end_cpu_read(*fd);
        }
    }
}

/// Mappings of the capture buffers, kept for the whole capture session. libcamera recycles a
/// handful of buffers, so mapping each once avoids an mmap plus page faults on every frame
/// (~0.3 ms per 1 MB plane on a CM5); frames still bracket their reads with dma-buf syncs.
#[derive(Default)]
pub(super) struct MappingCache {
    maps: Mutex<HashMap<SmallVec<[BackingPlaneView; 3]>, Arc<LazyMappedBackingState>>>,
}

impl MappingCache {
    fn map(&self, planes: &SmallVec<[BackingPlaneView; 3]>) -> Option<Arc<LazyMappedBackingState>> {
        let mut maps = self.maps.lock();
        if let Some(state) = maps.get(planes) {
            return Some(state.clone());
        }
        let state = Arc::new(map_backing_planes(planes)?);
        maps.insert(planes.clone(), state.clone());
        Some(state)
    }
}

impl Drop for LazyMappedBackingState {
    fn drop(&mut self) {
        for (_fd, range) in self.mmaps.drain(..).filter(|(_, r)| r.len > 0) {
            // SAFETY: each range was returned by `mmap64` in `map_backing_planes` with the same
            // pointer and length, and each successful mapping is stored exactly once.
            unsafe {
                libc::munmap(range.ptr, range.len);
            }
        }
    }
}

fn unique_backing_plane_bytes(planes: &[BackingPlaneView]) -> usize {
    let mut seen = SmallVec::<[(i32, usize, usize); 4]>::new();
    planes
        .iter()
        .filter(|plane| {
            let key = (plane.fd, plane.offset, plane.len);
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        })
        .map(|plane| plane.len)
        .sum()
}

fn framebuffer_backing_planes(buffer: &dyn AsFrameBuffer) -> SmallVec<[BackingPlaneView; 3]> {
    let planes = buffer.planes();
    let mut views = SmallVec::<[BackingPlaneView; 3]>::with_capacity(planes.len());
    for idx in 0..planes.len() {
        let Some(plane) = planes.get(idx) else {
            break;
        };
        views.push(BackingPlaneView {
            fd: plane.fd(),
            offset: plane.offset().unwrap_or(0),
            len: plane.len(),
        });
    }
    views
}

fn framebuffers_backing_planes(buffers: &[&dyn AsFrameBuffer]) -> SmallVec<[BackingPlaneView; 12]> {
    let mut views = SmallVec::<[BackingPlaneView; 12]>::new();
    for buffer in buffers {
        views.extend(framebuffer_backing_planes(*buffer));
    }
    views
}

/// Identity of the file behind `fd` (device, inode) and its size, or `None` if `fstat` fails.
fn file_identity(fd: i32) -> Option<((u64, u64), usize)> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `st` points to valid writable storage for `fstat` to initialize.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `assume_init` is reached only after `fstat` reports success.
    let st = unsafe { st.assume_init() };
    // `dev_t`/`ino_t` widths differ between targets.
    #[allow(clippy::unnecessary_cast)]
    Some(((st.st_dev as u64, st.st_ino as u64), st.st_size as usize))
}

/// Map the planes' buffers: one read-only mapping per dma-buf, spanning its planes. Planes on
/// duplicates of one buffer's fd (libcamera's planes each hold one) share the mapping.
fn map_backing_planes(planes: &[BackingPlaneView]) -> Option<LazyMappedBackingState> {
    struct MapInfo {
        fd: i32,
        file: Option<(u64, u64)>,
        start: usize,
        end: usize,
        total_len: usize,
    }

    let page_size = system_page_size();
    let mut map_info = SmallVec::<[MapInfo; 3]>::new();
    let mut plane_maps = SmallVec::<[u8; 3]>::new();
    for plane in planes {
        let end = plane.offset.checked_add(plane.len)?;
        let (file, total_len) = match file_identity(plane.fd) {
            Some((file, len)) => (Some(file), len),
            None => (None, 0),
        };
        let index = match map_info
            .iter()
            .position(|i| i.fd == plane.fd || (file.is_some() && i.file == file))
        {
            Some(index) => index,
            None => {
                map_info.push(MapInfo {
                    fd: plane.fd,
                    file,
                    start: plane.offset,
                    end,
                    total_len,
                });
                map_info.len() - 1
            }
        };
        let info = &mut map_info[index];
        if info.total_len > 0 && end > info.total_len {
            return None;
        }
        let aligned_start = plane.offset - (plane.offset % page_size);
        info.start = info.start.min(aligned_start);
        info.end = info.end.max(end);
        plane_maps.push(u8::try_from(index).ok()?);
    }

    let mut mapped_bytes = 0usize;
    let mut mmaps = SmallVec::<[(i32, MappedPlaneRange); 3]>::new();
    for info in map_info {
        let map_len = info.end.saturating_sub(info.start);
        if map_len == 0 {
            // Keep the indices of `plane_maps` valid: an empty plane maps nothing.
            mmaps.push((
                info.fd,
                MappedPlaneRange {
                    ptr: std::ptr::NonNull::<u8>::dangling().as_ptr().cast(),
                    len: 0,
                    map_offset: info.start,
                },
            ));
            continue;
        }
        // SAFETY: the fd comes from libcamera framebuffer metadata, the offset is page-aligned, and
        // `map_len` spans validated plane ranges. A failed mapping is detected via `MAP_FAILED`.
        let addr = unsafe {
            libc::mmap64(
                core::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                info.fd,
                info.start as _,
            )
        };
        if addr == libc::MAP_FAILED {
            // Unmap what was mapped so far.
            drop(LazyMappedBackingState {
                mmaps,
                plane_maps,
                mapped_bytes,
            });
            return None;
        }
        styx_core::metrics::frame_mapped();
        mapped_bytes = mapped_bytes.saturating_add(map_len);
        mmaps.push((
            info.fd,
            MappedPlaneRange {
                ptr: addr,
                len: map_len,
                map_offset: info.start,
            },
        ));
    }

    Some(LazyMappedBackingState {
        mmaps,
        plane_maps,
        mapped_bytes,
    })
}

fn prefault_backing_planes(planes: &[BackingPlaneView]) {
    let Some(mapped) = map_backing_planes(planes) else {
        return;
    };
    let page_size = system_page_size();
    let mut touched = 0u8;
    for (_, range) in mapped.mmaps.iter() {
        let ptr = range.ptr.cast::<u8>();
        let mut offset = 0usize;
        while offset < range.len {
            // SAFETY: `ptr..ptr+range.len` is a live read-only mmap held by `mapped`; offsets stay
            // within that range and volatile reads are used only to prefault pages.
            unsafe {
                touched ^= std::ptr::read_volatile(ptr.add(offset));
            }
            offset = offset.saturating_add(page_size);
        }
        if range.len > 0 {
            // SAFETY: `range.len > 0`, so `range.len - 1` is the last valid byte in the mapping.
            unsafe {
                touched ^= std::ptr::read_volatile(ptr.add(range.len - 1));
            }
        }
    }
    std::hint::black_box(touched);
}

pub(super) struct RequestPoolBackingLease {
    tracker: Arc<ExternalBackingTracker>,
    buffers: usize,
    bytes: usize,
}

impl RequestPoolBackingLease {
    pub(super) fn new(
        tracker: Arc<ExternalBackingTracker>,
        framebuffers: &[&dyn AsFrameBuffer],
        prefault_request_pools: bool,
    ) -> Self {
        let buffers = framebuffers.len();
        let planes = framebuffers_backing_planes(framebuffers);
        let bytes = unique_backing_plane_bytes(&planes);
        tracker.acquire_many(buffers, bytes);
        if prefault_request_pools && !planes.is_empty() {
            prefault_backing_planes(&planes);
        }
        Self {
            tracker,
            buffers,
            bytes,
        }
    }
}

impl Drop for RequestPoolBackingLease {
    fn drop(&mut self) {
        self.tracker.release_many(self.buffers, self.bytes);
    }
}

/// One request's way back to the capture worker, made once per request when capture starts
/// and reused by every frame its buffers carry, so a frame allocates no return record of its
/// own (nor a channel message). A completed request is put in the slot with the frame's
/// backing; once every backing of that frame (the frame and its companion) is dropped, the
/// slot marks it returned and the worker takes it back ([`RequestSlot::take_returned`]).
pub(super) struct RequestSlot<R = libcamera::request::Request> {
    req: Mutex<Option<R>>,
    /// Backings using the request now.
    holders: AtomicUsize,
    /// Every backing let go: the worker may take the request back.
    returned: AtomicBool,
    shutting_down: Arc<AtomicBool>,
    outstanding_backings: Arc<AtomicUsize>,
}

impl<R> RequestSlot<R> {
    pub(super) fn new(
        shutting_down: Arc<AtomicBool>,
        outstanding_backings: Arc<AtomicUsize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            req: Mutex::new(None),
            holders: AtomicUsize::new(0),
            returned: AtomicBool::new(false),
            shutting_down,
            outstanding_backings,
        })
    }

    /// The request, once every frame it backed was dropped (each returned request once).
    pub(super) fn take_returned(&self) -> Option<R> {
        if !self.returned.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.req.lock().take()
    }

    /// The completed request `req` is now backing a frame.
    fn fill(&self, req: R) {
        let previous = self.req.lock().replace(req);
        debug_assert!(previous.is_none(), "a request slot reused while held");
        self.outstanding_backings.fetch_add(1, Ordering::AcqRel);
        self.hold();
    }

    fn hold(&self) {
        self.holders.fetch_add(1, Ordering::AcqRel);
    }

    /// One backing let go; the last one returns the request (unless capture is shutting down).
    fn release(&self) {
        if self.holders.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        if self.shutting_down.load(Ordering::Acquire) {
            drop(self.req.lock().take());
        } else {
            self.returned.store(true, Ordering::Release);
        }
        self.outstanding_backings.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct LibcameraBacking<R: Send + 'static = libcamera::request::Request> {
    request: Arc<RequestSlot<R>>,
    planes: SmallVec<[BackingPlaneView; 3]>,
    cache: Arc<MappingCache>,
    mapped: OnceLock<Option<Arc<LazyMappedBackingState>>>,
    /// CPU access to the mapped buffers (`DMA_BUF_IOCTL_SYNC`): held by plain reads until the
    /// backing drops, or bracketed (Daedalus's `daedalus:frame` access).
    window: styx_core::buffer::CpuReadWindow,
    outstanding_tracker: Arc<ExternalBackingTracker>,
    mapped_tracker: Arc<ExternalBackingTracker>,
    backing_bytes: usize,
    /// Buffers from a cached dma-heap rather than libcamera's allocator.
    cached: bool,
}

impl<R: Send + 'static> LibcameraBacking<R> {
    /// The backing of a completed request's buffer: `req` waits in `slot` (the request's own)
    /// until the frame, and every sibling, is dropped. One allocation: the backing itself.
    pub(super) fn new(
        slot: &Arc<RequestSlot<R>>,
        req: R,
        planes: SmallVec<[BackingPlaneView; 3]>,
        cache: Arc<MappingCache>,
        outstanding_tracker: Arc<ExternalBackingTracker>,
        mapped_tracker: Arc<ExternalBackingTracker>,
        cached: bool,
    ) -> Arc<Self> {
        slot.fill(req);
        Self::with_request(
            slot.clone(),
            planes,
            cache,
            outstanding_tracker,
            mapped_tracker,
            cached,
        )
    }

    /// A backing for another stream's buffer in the same request (e.g. a pyramid companion).
    /// The request is requeued only after both backings are dropped.
    pub(super) fn sibling(&self, planes: SmallVec<[BackingPlaneView; 3]>) -> Arc<Self> {
        self.request.hold();
        Self::with_request(
            self.request.clone(),
            planes,
            self.cache.clone(),
            self.outstanding_tracker.clone(),
            self.mapped_tracker.clone(),
            self.cached,
        )
    }

    fn with_request(
        request: Arc<RequestSlot<R>>,
        planes: SmallVec<[BackingPlaneView; 3]>,
        cache: Arc<MappingCache>,
        outstanding_tracker: Arc<ExternalBackingTracker>,
        mapped_tracker: Arc<ExternalBackingTracker>,
        cached: bool,
    ) -> Arc<Self> {
        let backing_bytes = unique_backing_plane_bytes(&planes);
        outstanding_tracker.acquire_many(1, backing_bytes);
        Arc::new(Self {
            request,
            planes,
            cache,
            mapped: OnceLock::new(),
            window: styx_core::buffer::CpuReadWindow::new(),
            outstanding_tracker,
            mapped_tracker,
            backing_bytes,
            cached,
        })
    }

    /// Plane `index`'s bytes in the buffers' mappings (made on first use, cached across frames),
    /// no sync.
    fn mapped_plane(&self, index: usize) -> Option<(&LazyMappedBackingState, &[u8])> {
        let plane = self.planes.get(index)?;
        let mapped = self.mapped_state()?;
        let map = usize::from(*mapped.plane_maps.get(index)?);
        let (_, range) = mapped.mmaps.get(map)?;
        if plane.len == 0 {
            return Some((mapped, &[]));
        }
        let offset = plane.offset.checked_sub(range.map_offset)?;
        if offset.checked_add(plane.len)? > range.len {
            return None;
        }
        let ptr: *const u8 = range.ptr.cast();
        // SAFETY: `mapped_state` keeps the mmap alive for `self`, and `offset..offset+len` was
        // just checked to lie within the plane's mapping.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.add(offset), plane.len) };
        Some((mapped, bytes))
    }

    fn mapped_state(&self) -> Option<&LazyMappedBackingState> {
        self.mapped
            .get_or_init(|| {
                let mapped = self.cache.map(&self.planes);
                if let Some(state) = mapped.as_ref() {
                    self.mapped_tracker.acquire(state.mapped_bytes);
                }
                mapped
            })
            .as_deref()
    }

    /// Duplicates of the planes' fds, appended to `out`.
    fn export_planes(&self, out: &mut Vec<FrameFdPlane>) -> Result<(), FrameExportError> {
        for plane in &self.planes {
            // SAFETY: duplicating an owned libcamera plane fd does not take ownership of the
            // original. A negative return is handled as an OS error.
            let fd = unsafe { libc::dup(plane.fd) };
            if fd < 0 {
                return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
            }
            out.push(FrameFdPlane {
                // SAFETY: `dup` returned a fresh non-negative fd, transferring ownership into
                // `OwnedFd` exactly once.
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                offset: plane.offset,
                len: plane.len,
            });
        }
        Ok(())
    }
}

// SAFETY: the backing owns the libcamera request return path and lazily maps planes for immutable
// reads. Moving the backing between threads does not duplicate ownership, and drop returns the
// request after mappings are released.
unsafe impl<R: Send + 'static> Send for LibcameraBacking<R> {}

// SAFETY: lazy mapping is synchronized by `OnceLock`, exposed plane data is immutable, and request
// shutdown/outstanding counters are synchronized by their own atomics/channels.
unsafe impl<R: Send + 'static> Sync for LibcameraBacking<R> {}

impl<R: Send + 'static> ExternalBacking for LibcameraBacking<R> {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let (mapped, bytes) = self.mapped_plane(index)?;
        self.window.hold(|| mapped.begin_cpu_read());
        Some(bytes)
    }

    fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        let (mapped, bytes) = self.mapped_plane(index)?;
        self.window.begin(|| mapped.begin_cpu_read());
        Some(bytes)
    }

    fn end_cpu_read(&self, _index: usize) {
        if let Some(Some(mapped)) = self.mapped.get() {
            self.window.end(|| mapped.end_cpu_read());
        }
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.backing_bytes)
    }

    fn backing_kind(&self) -> &'static str {
        "libcamera_dmabuf"
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::Dmabuf
    }

    fn dmabuf_plane(&self, index: usize) -> Option<styx_core::buffer::DmabufPlane<'_>> {
        let plane = self.planes.get(index)?;
        (plane.fd >= 0).then(|| styx_core::buffer::DmabufPlane {
            // SAFETY: libcamera's frame buffer owns the descriptor and outlives this backing,
            // which keeps the request (and with it the buffer) until it drops.
            fd: unsafe { std::os::fd::BorrowedFd::borrow_raw(plane.fd) },
            offset: plane.offset,
        })
    }

    fn cpu_access(&self) -> CpuAccess {
        if self.cached {
            CpuAccess::Cached
        } else {
            CpuAccess::Uncached
        }
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let mut planes = Vec::with_capacity(self.planes.len());
        self.export_planes(&mut planes)?;
        Ok(Some(FrameBackingExport::DmabufPlanes { planes }))
    }

    /// Into the sender's list (a frame socket's or a camera service's, reused): no list of
    /// its own per frame.
    fn export_into(
        &self,
        out: &mut Vec<FrameFdPlane>,
    ) -> Result<Option<ExportedKind>, FrameExportError> {
        self.export_planes(out)?;
        Ok(Some(ExportedKind::DmabufPlanes))
    }
}

impl<R: Send + 'static> Drop for LibcameraBacking<R> {
    fn drop(&mut self) {
        // End the CPU read before the request can be requeued; the mapping itself stays cached
        // for the next frame that uses this buffer.
        if let Some(mapped) = self.mapped.take().flatten() {
            self.window.close(|| mapped.end_cpu_read());
            self.mapped_tracker.release(mapped.mapped_bytes);
        }
        self.outstanding_tracker.release_many(1, self.backing_bytes);
        self.request.release();
    }
}

pub(super) struct ShutdownGuard(pub std::sync::Arc<AtomicBool>);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// How many mappings `planes` take (one per distinct buffer). For tests.
#[cfg(test)]
pub(super) fn mappings_for(planes: &[BackingPlaneView]) -> Option<usize> {
    map_backing_planes(planes).map(|m| m.mmaps.len())
}
