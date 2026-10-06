//! Mappings of the dma-bufs a client receives, kept across frames.
//!
//! A camera service sends the same few buffers over and over (a capture cycles through its
//! buffers), each time as new descriptors. Mapping a received buffer on every frame costs a
//! `mmap`, a page fault (or a populate) per page and a `munmap` with its TLB flush: about
//! 0.5 ms per 1280x800 NV12 frame on a Cortex-A76, more than reading it. The client keeps the
//! mapping of each buffer it has read (keyed by the buffer's identity, so descriptors
//! duplicated from one dma-buf share it) and maps only buffers it has not seen. Entries no
//! frame used for a while are dropped, so the buffers of a capture that was restarted are not
//! kept alive for long.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use smallvec::SmallVec;
use styx_core::buffer::CpuReadWindow;
use styx_core::prelude::*;

/// Imports after which an entry no frame used is dropped (a few seconds at camera rates).
const MAX_AGE: u64 = 120;
/// Mappings kept at most.
const MAX_ENTRIES: usize = 32;

/// A read-only mapping of a whole buffer.
pub(super) struct Map {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the mapping is read-only and only handed out as shared slices; it is unmapped once,
// in `Drop`, after every `Arc` holder is gone.
unsafe impl Send for Map {}
// SAFETY: see `Send`: shared reads only.
unsafe impl Sync for Map {}

impl Map {
    fn new(fd: BorrowedFd<'_>, len: usize) -> Option<Self> {
        // SAFETY: a fresh read-only shared mapping of `len` bytes of a descriptor we hold;
        // failure is checked.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let flags = libc::MAP_SHARED | libc::MAP_POPULATE;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let flags = libc::MAP_SHARED;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                flags,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        styx_core::metrics::frame_mapped();
        Some(Self { ptr, len })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` readable bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: unmaps the mapping made in `Map::new`, once.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

struct Entry {
    id: (u64, u64),
    map: Arc<Map>,
    last: u64,
}

/// See the [module documentation](self).
pub(super) struct MapCache {
    inner: Mutex<(u64, Vec<Entry>)>,
}

impl Default for MapCache {
    /// With room for the entries kept (and as many mapped since the last trim), so that a
    /// buffer mapped for the first time costs its one allocation (the shared mapping), not a
    /// growing list besides.
    fn default() -> Self {
        Self {
            inner: Mutex::new((0, Vec::with_capacity(2 * MAX_ENTRIES))),
        }
    }
}

/// Device and inode of the buffer behind `fd`.
fn identity(fd: BorrowedFd<'_>) -> Option<(u64, u64)> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` fills the buffer for a valid descriptor; it is read only on success.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `fstat` succeeded.
    let st = unsafe { st.assume_init() };
    // The field types differ between platforms.
    #[allow(clippy::unnecessary_cast)]
    Some((st.st_dev as u64, st.st_ino as u64))
}

/// The length of the file (memfd) behind `fd`.
fn file_size(fd: BorrowedFd<'_>) -> Option<u64> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` fills the buffer for a valid descriptor; it is read only on success.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `fstat` succeeded.
    let st = unsafe { st.assume_init() };
    u64::try_from(st.st_size).ok()
}

impl MapCache {
    /// Counts an imported frame and drops entries that went unused for a while.
    fn tick(&self) {
        let mut g = self.inner.lock();
        g.0 += 1;
        let now = g.0;
        g.1.retain(|e| e.last + MAX_AGE > now || Arc::strong_count(&e.map) > 1);
        if g.1.len() > MAX_ENTRIES {
            g.1.sort_by_key(|e| std::cmp::Reverse(e.last));
            g.1.truncate(MAX_ENTRIES);
        }
    }

    /// The mapping of the buffer `id` (at least `len` bytes), made from `fd` if new.
    fn map(&self, id: (u64, u64), fd: BorrowedFd<'_>, len: usize) -> Option<Arc<Map>> {
        let mut g = self.inner.lock();
        let now = g.0;
        if let Some(e) = g.1.iter_mut().find(|e| e.id == id && e.map.len >= len) {
            e.last = now;
            return Some(e.map.clone());
        }
        drop(g);
        let map = Arc::new(Map::new(fd, len)?);
        let mut g = self.inner.lock();
        g.1.retain(|e| e.id != id);
        g.1.push(Entry {
            id,
            map: map.clone(),
            last: now,
        });
        Some(map)
    }
}

/// A received dma-buf frame whose planes are all on one buffer, mapped through the cache on
/// first read.
pub(super) struct CachedDmabuf {
    cache: Arc<MapCache>,
    id: (u64, u64),
    /// A memfd (one descriptor, every plane at its layout offset in it; no cache maintenance)
    /// rather than dma-buf planes.
    memfd: bool,
    fds: Fds,
    planes: SmallVec<[(usize, usize); 3]>,
    map: OnceLock<Option<Arc<Map>>>,
    /// CPU access to the dma-buf (`DMA_BUF_IOCTL_SYNC`; none for a memfd): held by plain reads
    /// until the frame drops, or bracketed (Daedalus's `daedalus:frame` access).
    window: CpuReadWindow,
}

/// A frame's descriptors, inline (no allocation per frame).
pub(super) type Fds = SmallVec<[OwnedFd; 4]>;

impl CachedDmabuf {
    /// The planes (each `fds[i]` holding `spans[i]`, an offset and a length), when every
    /// descriptor refers to one buffer; the descriptors back otherwise (or when one cannot be
    /// inspected).
    pub(super) fn new(
        cache: &Arc<MapCache>,
        fds: Fds,
        spans: &[(usize, usize)],
    ) -> Result<Self, Fds> {
        let mut id = None;
        for fd in &fds {
            match (identity(fd.as_fd()), id) {
                (Some(this), None) => id = Some(this),
                (Some(this), Some(first)) if this == first => {}
                _ => return Err(fds),
            }
        }
        let Some(id) = id else {
            return Err(fds);
        };
        cache.tick();
        Ok(Self {
            cache: cache.clone(),
            id,
            memfd: false,
            fds,
            planes: spans.iter().copied().collect(),
            map: OnceLock::new(),
            window: CpuReadWindow::new(),
        })
    }

    /// A memfd of `len` bytes holding `planes` planes (each at its layout offset), read through
    /// the cache as dma-bufs are; the descriptor back when it cannot be inspected.
    pub(super) fn memfd(
        cache: &Arc<MapCache>,
        fd: OwnedFd,
        len: usize,
        planes: usize,
    ) -> Result<Self, OwnedFd> {
        // A memfd shorter than the message says would fault on reading past its end.
        if file_size(fd.as_fd()).is_none_or(|size| (len as u64) > size) {
            return Err(fd);
        }
        let mut fds = Fds::new();
        fds.push(fd);
        let spans: SmallVec<[(usize, usize); 3]> = (0..planes.max(1)).map(|_| (0, len)).collect();
        match Self::new(cache, fds, &spans) {
            Ok(mut cached) => {
                cached.memfd = true;
                Ok(cached)
            }
            Err(fds) => Err(fds.into_iter().next().expect("one descriptor")),
        }
    }

    /// Plane `index` in the buffer's mapping (made through the cache on first use), no sync.
    fn bytes(&self, index: usize) -> Option<&[u8]> {
        let &(offset, len) = self.planes.get(index)?;
        self.mapped()?.bytes().get(offset..offset.checked_add(len)?)
    }

    /// Starts (`true`) or ends CPU reads of the dma-buf; nothing for a memfd.
    fn sync(&self, start: bool) {
        if self.memfd {
            return;
        }
        if let Some(fd) = self.fds.first() {
            let _ = if start {
                styx_core::buffer::dmabuf_begin_cpu_read(fd.as_raw_fd())
            } else {
                styx_core::buffer::dmabuf_end_cpu_read(fd.as_raw_fd())
            };
        }
    }

    fn mapped(&self) -> Option<&Map> {
        self.map
            .get_or_init(|| {
                let len = self.planes.iter().map(|(o, l)| o + l).max()?;
                let page = 4096;
                let map = self.cache.map(
                    self.id,
                    self.fds.first()?.as_fd(),
                    len.next_multiple_of(page),
                )?;
                Some(map)
            })
            .as_deref()
    }
}

impl ExternalBacking for CachedDmabuf {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let bytes = self.bytes(index)?;
        self.window.hold(|| self.sync(true));
        Some(bytes)
    }

    fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        let bytes = self.bytes(index)?;
        self.window.begin(|| self.sync(true));
        Some(bytes)
    }

    fn end_cpu_read(&self, _index: usize) {
        self.window.end(|| self.sync(false));
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.planes.iter().map(|(o, l)| o + l).max()
    }

    fn backing_kind(&self) -> &'static str {
        if self.memfd { "memfd_import" } else { "dmabuf" }
    }

    fn can_export(&self) -> bool {
        true
    }

    fn residency(&self) -> FrameResidency {
        if self.memfd {
            FrameResidency::HostExternal
        } else {
            FrameResidency::Dmabuf
        }
    }

    fn dmabuf_plane(&self, index: usize) -> Option<styx_core::buffer::DmabufPlane<'_>> {
        if self.memfd {
            return None;
        }
        let &(offset, _) = self.planes.get(index)?;
        let fd = self.fds.get(index).or(self.fds.first())?;
        Some(styx_core::buffer::DmabufPlane {
            fd: fd.as_fd(),
            offset,
        })
    }

    fn cpu_access(&self) -> CpuAccess {
        // Mapped here; the sender says whether its memory is cached (see `Released`).
        if self.memfd {
            CpuAccess::Cached
        } else {
            CpuAccess::Uncached
        }
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        if self.memfd {
            let fd = self.fds[0].try_clone().map_err(FrameExportError::Fd)?;
            let len = self.backing_bytes().unwrap_or(0);
            return Ok(Some(FrameBackingExport::Memfd { fd, len }));
        }
        let planes = self
            .fds
            .iter()
            .zip(&self.planes)
            .map(|(fd, &(offset, len))| {
                Ok(FrameFdPlane {
                    fd: fd.try_clone().map_err(FrameExportError::Fd)?,
                    offset,
                    len,
                })
            })
            .collect::<Result<Vec<_>, FrameExportError>>()?;
        Ok(Some(FrameBackingExport::DmabufPlanes { planes }))
    }
}

impl CachedDmabuf {
    /// [`ExternalBacking::export_into`]: the descriptors duplicated, without a list of its own.
    pub(super) fn export_fds(
        &self,
        out: &mut Vec<FrameFdPlane>,
    ) -> Result<ExportedKind, FrameExportError> {
        if self.memfd {
            let fd = self.fds[0].try_clone().map_err(FrameExportError::Fd)?;
            let len = self.backing_bytes().unwrap_or(0);
            out.push(FrameFdPlane { fd, offset: 0, len });
            return Ok(ExportedKind::Memfd);
        }
        for (fd, &(offset, len)) in self.fds.iter().zip(&self.planes) {
            let fd = fd.try_clone().map_err(FrameExportError::Fd)?;
            out.push(FrameFdPlane { fd, offset, len });
        }
        Ok(ExportedKind::DmabufPlanes)
    }
}

impl Drop for CachedDmabuf {
    fn drop(&mut self) {
        self.window.close(|| self.sync(false));
    }
}

/// A received frame's memory.
pub(super) enum Inner {
    /// One buffer (a dma-buf or a memfd), read through the receiver's mapping cache.
    Cached(CachedDmabuf),
    /// Anything else (planes on several buffers, descriptors that could not be inspected).
    Other(Arc<dyn ExternalBacking>),
}

impl Inner {
    pub(super) fn backing(&self) -> &dyn ExternalBacking {
        match self {
            Inner::Cached(c) => c,
            Inner::Other(o) => o.as_ref(),
        }
    }

    /// [`ExternalBacking::export_into`] of the memory.
    pub(super) fn export_into(
        &self,
        out: &mut Vec<FrameFdPlane>,
    ) -> Result<Option<ExportedKind>, FrameExportError> {
        match self {
            Inner::Cached(c) => c.export_fds(out).map(Some),
            Inner::Other(o) => o.export_into(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::FromRawFd;

    use super::*;

    fn memfd(bytes: &[u8]) -> OwnedFd {
        let name = std::ffi::CString::new("mapcache").unwrap();
        // SAFETY: creates a new descriptor we own.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: `fd` is a new descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        std::fs::File::from(fd.try_clone().unwrap())
            .write_all(bytes)
            .unwrap();
        fd
    }

    #[test]
    fn planes_of_one_buffer_share_a_mapping_across_frames() {
        let mut bytes = vec![1u8; 8192];
        bytes[4096..].fill(2);
        let fd = memfd(&bytes);
        let cache = Arc::new(MapCache::default());
        let planes = |fd: &OwnedFd| -> Fds {
            [fd.try_clone().unwrap(), fd.try_clone().unwrap()]
                .into_iter()
                .collect()
        };
        let spans = [(0, 4096), (4096, 4096)];
        let a = CachedDmabuf::new(&cache, planes(&fd), &spans).ok().unwrap();
        assert!(a.plane_data(0).unwrap().iter().all(|&v| v == 1));
        assert!(a.plane_data(1).unwrap().iter().all(|&v| v == 2));
        let b = CachedDmabuf::new(&cache, planes(&fd), &spans).ok().unwrap();
        assert_eq!(b.plane_data(1).unwrap()[0], 2);
        // One mapping, used by both frames.
        assert_eq!(cache.inner.lock().1.len(), 1);
        assert!(std::ptr::eq(a.mapped().unwrap(), b.mapped().unwrap()));
        // Planes on two buffers are not this backing's.
        let other = memfd(&bytes);
        let mut mixed = planes(&fd);
        mixed[1] = other.try_clone().unwrap();
        assert!(CachedDmabuf::new(&cache, mixed, &spans).is_err());
        // Exports keep the plane offsets.
        let Some(FrameBackingExport::DmabufPlanes { planes: out }) = a.export_backing().unwrap()
        else {
            panic!("no export");
        };
        assert_eq!(out[1].offset, 4096);
        drop((a, b));
        // Unused entries go after a while.
        for _ in 0..=MAX_AGE {
            cache.tick();
        }
        assert!(cache.inner.lock().1.is_empty());
    }
}
