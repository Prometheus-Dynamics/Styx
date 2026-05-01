use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use libcamera::framebuffer::AsFrameBuffer;
use libcamera::framebuffer_allocator::FrameBuffer;
use smallvec::SmallVec;
use std::os::fd::{FromRawFd, OwnedFd};
use styx_core::prelude::{
    ExternalBacking, FrameBackingExport, FrameExportError, FrameFdPlane, FrameResidency,
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
            tracing::debug!(
                backend = "libcamera",
                idle_drain_ms = start.elapsed().as_millis() as u64,
                "libcamera external backings drained"
            );
            return true;
        }
        if start.elapsed() >= timeout {
            tracing::debug!(
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
    mmaps: SmallVec<[(i32, MappedPlaneRange); 3]>,
    mapped_bytes: usize,
}

impl Drop for LazyMappedBackingState {
    fn drop(&mut self) {
        for (_fd, range) in self.mmaps.drain(..) {
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

fn framebuffer_backing_planes(buffer: &FrameBuffer) -> SmallVec<[BackingPlaneView; 3]> {
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

fn framebuffers_backing_planes(buffers: &[FrameBuffer]) -> SmallVec<[BackingPlaneView; 12]> {
    let mut views = SmallVec::<[BackingPlaneView; 12]>::new();
    for buffer in buffers {
        views.extend(framebuffer_backing_planes(buffer));
    }
    views
}

fn map_backing_planes(planes: &[BackingPlaneView]) -> Option<LazyMappedBackingState> {
    struct MapInfo {
        start: usize,
        end: usize,
        total_len: usize,
    }

    let page_size = system_page_size();
    let mut map_info = SmallVec::<[(i32, MapInfo); 3]>::new();
    for plane in planes {
        let end = plane.offset.checked_add(plane.len)?;
        let info = if let Some((_, info)) = map_info.iter_mut().find(|(fd, _)| *fd == plane.fd) {
            info
        } else {
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `st` points to valid writable storage for `fstat` to initialize.
            let ret = unsafe { libc::fstat(plane.fd, st.as_mut_ptr()) };
            let total_len = if ret != 0 {
                0
            } else {
                // SAFETY: `assume_init` is reached only after `fstat` reports success.
                let st = unsafe { st.assume_init() };
                st.st_size as usize
            };
            map_info.push((
                plane.fd,
                MapInfo {
                    start: plane.offset,
                    end,
                    total_len,
                },
            ));
            &mut map_info.last_mut().expect("map info entry just pushed").1
        };

        if info.total_len > 0 && end > info.total_len {
            return None;
        }

        let aligned_start = plane.offset - (plane.offset % page_size);
        info.start = info.start.min(aligned_start);
        info.end = info.end.max(end);
    }

    let mut mapped_bytes = 0usize;
    let mut mmaps = SmallVec::<[(i32, MappedPlaneRange); 3]>::new();
    for (fd, info) in map_info {
        let map_len = info.end.saturating_sub(info.start);
        if map_len == 0 {
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
                fd,
                info.start as _,
            )
        };
        if addr == libc::MAP_FAILED {
            return None;
        }
        mapped_bytes = mapped_bytes.saturating_add(map_len);
        mmaps.push((
            fd,
            MappedPlaneRange {
                ptr: addr,
                len: map_len,
                map_offset: info.start,
            },
        ));
    }

    Some(LazyMappedBackingState {
        mmaps,
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
        framebuffers: &[FrameBuffer],
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

pub(super) struct LibcameraBacking {
    req: std::sync::Mutex<Option<libcamera::request::Request>>,
    planes: SmallVec<[BackingPlaneView; 3]>,
    mapped: OnceLock<Option<LazyMappedBackingState>>,
    ret_tx: std::sync::mpsc::Sender<libcamera::request::Request>,
    shutting_down: std::sync::Arc<AtomicBool>,
    outstanding_backings: Arc<AtomicUsize>,
    tracker: Arc<ExternalBackingTracker>,
    backing_bytes: usize,
}

impl LibcameraBacking {
    pub(super) fn new(
        req: libcamera::request::Request,
        ret_tx: std::sync::mpsc::Sender<libcamera::request::Request>,
        planes: SmallVec<[BackingPlaneView; 3]>,
        shutting_down: std::sync::Arc<AtomicBool>,
        outstanding_backings: Arc<AtomicUsize>,
        tracker: Arc<ExternalBackingTracker>,
    ) -> std::sync::Arc<Self> {
        let backing_bytes = unique_backing_plane_bytes(&planes);
        outstanding_backings.fetch_add(1, Ordering::AcqRel);
        std::sync::Arc::new(Self {
            req: std::sync::Mutex::new(Some(req)),
            planes,
            mapped: OnceLock::new(),
            ret_tx,
            shutting_down,
            outstanding_backings,
            tracker,
            backing_bytes,
        })
    }

    fn mapped_state(&self) -> Option<&LazyMappedBackingState> {
        self.mapped
            .get_or_init(|| {
                let mapped = map_backing_planes(&self.planes);
                if let Some(state) = mapped.as_ref() {
                    self.tracker.acquire(state.mapped_bytes);
                }
                mapped
            })
            .as_ref()
    }
}

// SAFETY: the backing owns the libcamera request return path and lazily maps planes for immutable
// reads. Moving the backing between threads does not duplicate ownership, and drop returns the
// request after mappings are released.
unsafe impl Send for LibcameraBacking {}

// SAFETY: lazy mapping is synchronized by `OnceLock`, exposed plane data is immutable, and request
// shutdown/outstanding counters are synchronized by their own atomics/channels.
unsafe impl Sync for LibcameraBacking {}

impl ExternalBacking for LibcameraBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let plane = self.planes.get(index)?;
        let mapped = self.mapped_state()?;
        let (_, range) = mapped.mmaps.iter().find(|(fd, _)| *fd == plane.fd)?;
        let offset = plane.offset.checked_sub(range.map_offset)?;
        let ptr: *const u8 = range.ptr.cast();
        // SAFETY: `mapped_state` keeps the mmap alive for `self`, `offset` is within the mapped
        // range selected for this fd, and `plane.len` was validated before mapping.
        Some(unsafe { std::slice::from_raw_parts(ptr.add(offset), plane.len) })
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

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let mut planes = Vec::with_capacity(self.planes.len());
        for plane in &self.planes {
            // SAFETY: duplicating an owned libcamera plane fd does not take ownership of the
            // original. A negative return is handled as an OS error.
            let fd = unsafe { libc::dup(plane.fd) };
            if fd < 0 {
                return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
            }
            planes.push(FrameFdPlane {
                // SAFETY: `dup` returned a fresh non-negative fd, transferring ownership into
                // `OwnedFd` exactly once.
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                offset: plane.offset,
                len: plane.len,
            });
        }
        Ok(Some(FrameBackingExport::DmabufPlanes { planes }))
    }
}

impl Drop for LibcameraBacking {
    fn drop(&mut self) {
        if let Some(mapped) = self.mapped.take().flatten() {
            self.tracker.release(mapped.mapped_bytes);
            drop(mapped);
        }
        if self.shutting_down.load(Ordering::Acquire) {
            self.outstanding_backings.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        if let Some(req) = self.req.lock().unwrap().take() {
            let _ = self.ret_tx.send(req);
        }
        self.outstanding_backings.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct ShutdownGuard(pub std::sync::Arc<AtomicBool>);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
