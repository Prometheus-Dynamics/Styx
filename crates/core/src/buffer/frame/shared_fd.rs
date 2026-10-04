use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::{ExternalBacking, FrameBackingExport, FrameExportError, FrameFdPlane, FrameResidency};

pub(super) struct SharedFdBacking {
    kind: SharedFdBackingKind,
    planes: Vec<SharedFdPlane>,
    mapped: std::sync::OnceLock<Mapped>,
}

/// The mappings made on first CPU access: one per underlying buffer (planes whose descriptors
/// refer to the same buffer, e.g. the Y and CbCr planes of one NV12 dma-buf, share one mapping
/// and one CPU-access sync), and which mapping each plane is in.
struct Mapped {
    ranges: Vec<MappedFdRange>,
    /// The descriptor each range was mapped from (for the end-of-access sync).
    range_fds: Vec<usize>,
    plane_range: Vec<Option<usize>>,
}

enum SharedFdBackingKind {
    Memfd(OwnedFd),
    Dmabuf(Vec<OwnedFd>),
}

#[derive(Clone, Copy)]
struct SharedFdPlane {
    fd_index: usize,
    offset: usize,
    len: usize,
}

/// Planes on one buffer: its identity, the descriptor to map it from and the byte span the
/// planes cover.
type Group = (Option<(u64, u64)>, usize, usize, usize);

struct MappedFdRange {
    ptr: *mut core::ffi::c_void,
    map_len: usize,
    map_offset: usize,
}

impl SharedFdBacking {
    pub(super) fn memfd(fd: OwnedFd, len: usize, plane_count: usize) -> Self {
        Self {
            kind: SharedFdBackingKind::Memfd(fd),
            planes: vec![
                SharedFdPlane {
                    fd_index: 0,
                    offset: 0,
                    len,
                };
                plane_count
            ],
            mapped: std::sync::OnceLock::new(),
        }
    }

    pub(super) fn dmabuf(planes: Vec<FrameFdPlane>) -> Self {
        let mut fds = Vec::with_capacity(planes.len());
        let planes = planes
            .into_iter()
            .enumerate()
            .map(|(fd_index, plane)| {
                fds.push(plane.fd);
                SharedFdPlane {
                    fd_index,
                    offset: plane.offset,
                    len: plane.len,
                }
            })
            .collect();
        Self {
            kind: SharedFdBackingKind::Dmabuf(fds),
            planes,
            mapped: std::sync::OnceLock::new(),
        }
    }

    fn fd(&self, index: usize) -> Option<&OwnedFd> {
        match &self.kind {
            SharedFdBackingKind::Memfd(fd) if index == 0 => Some(fd),
            SharedFdBackingKind::Memfd(_) => None,
            SharedFdBackingKind::Dmabuf(fds) => fds.get(index),
        }
    }

    /// Identity of the buffer behind descriptor `index` (device and inode: descriptors
    /// duplicated from one dma-buf or memfd agree), `None` if it cannot be read.
    fn identity(&self, index: usize) -> Option<(u64, u64)> {
        let fd = self.fd(index)?;
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fstat` writes a `stat` into the buffer for a valid descriptor; it is read
        // only when the call succeeded.
        let r = unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) };
        if r != 0 {
            return None;
        }
        // SAFETY: `fstat` succeeded, so `st` is initialised.
        let st = unsafe { st.assume_init() };
        // The field types differ between platforms.
        #[allow(clippy::unnecessary_cast)]
        Some((st.st_dev as u64, st.st_ino as u64))
    }

    fn mapped(&self) -> &Mapped {
        self.mapped.get_or_init(|| {
            // Group the planes by the buffer behind their descriptors: the span each group
            // covers is mapped (and synced) once.
            let mut groups: Vec<Group> = Vec::new();
            let mut plane_group = Vec::with_capacity(self.planes.len());
            let mut ids: Vec<(usize, Option<(u64, u64)>)> = Vec::new();
            for plane in &self.planes {
                if plane.len == 0 || self.fd(plane.fd_index).is_none() {
                    plane_group.push(None);
                    continue;
                }
                let id = match ids.iter().find(|(i, _)| *i == plane.fd_index) {
                    Some((_, id)) => *id,
                    None => {
                        let id = self.identity(plane.fd_index);
                        ids.push((plane.fd_index, id));
                        id
                    }
                };
                let end = plane.offset.saturating_add(plane.len);
                let same = |g: &Group| {
                    if id.is_some() {
                        g.0 == id
                    } else {
                        g.1 == plane.fd_index
                    }
                };
                match groups.iter().position(same) {
                    Some(g) => {
                        groups[g].2 = groups[g].2.min(plane.offset);
                        groups[g].3 = groups[g].3.max(end);
                        plane_group.push(Some(g));
                    }
                    None => {
                        groups.push((id, plane.fd_index, plane.offset, end));
                        plane_group.push(Some(groups.len() - 1));
                    }
                }
            }
            let mut ranges = Vec::with_capacity(groups.len());
            let mut range_fds = Vec::with_capacity(groups.len());
            let mut group_range = Vec::with_capacity(groups.len());
            for &(_, fd_index, lo, hi) in &groups {
                match self.map_span(fd_index, lo, hi - lo) {
                    Ok(Some(range)) => {
                        group_range.push(Some(ranges.len()));
                        ranges.push(range);
                        range_fds.push(fd_index);
                    }
                    _ => group_range.push(None),
                }
            }
            let mapped = Mapped {
                ranges,
                range_fds,
                plane_range: plane_group
                    .into_iter()
                    .map(|g| g.and_then(|g| group_range[g]))
                    .collect(),
            };
            self.sync_dmabufs(&mapped.range_fds, true);
            mapped
        })
    }

    /// Bracket CPU reads of imported dma-bufs so cached mappings never serve stale lines (once
    /// per mapped buffer).
    fn sync_dmabufs(&self, fds: &[usize], begin: bool) {
        #[cfg(target_os = "linux")]
        if let SharedFdBackingKind::Dmabuf(all) = &self.kind {
            for fd in fds.iter().filter_map(|&i| all.get(i)) {
                let _ = if begin {
                    crate::buffer::dmabuf_begin_cpu_read(fd.as_raw_fd())
                } else {
                    crate::buffer::dmabuf_end_cpu_read(fd.as_raw_fd())
                };
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (fds, begin);
    }

    /// Maps `len` bytes at `offset` of descriptor `fd_index` read-only, populated (the pages
    /// are about to be read: one call instead of a fault per page).
    fn map_span(
        &self,
        fd_index: usize,
        offset: usize,
        len: usize,
    ) -> Result<Option<MappedFdRange>, FrameExportError> {
        if len == 0 {
            return Ok(None);
        }
        let Some(fd) = self.fd(fd_index) else {
            return Ok(None);
        };
        // Pages of a file mapped past its end fault (SIGBUS) when read: a memfd shorter than
        // its descriptor claims (sent by another process) must not be mapped. Dma-bufs refuse
        // such mappings themselves.
        if let Some(size) = regular_file_size(fd)
            && offset.checked_add(len).is_none_or(|end| end as u64 > size)
        {
            return Ok(None);
        }
        let page_size = system_page_size();
        let map_offset = offset - (offset % page_size);
        let delta = offset - map_offset;
        let map_len = delta.saturating_add(len);
        #[cfg(target_os = "linux")]
        let flags = libc::MAP_SHARED | libc::MAP_POPULATE;
        #[cfg(not(target_os = "linux"))]
        let flags = libc::MAP_SHARED;
        let addr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                flags,
                fd.as_raw_fd(),
                map_offset as _,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(FrameExportError::Mmap(std::io::Error::last_os_error()));
        }
        Ok(Some(MappedFdRange {
            ptr: addr,
            map_len,
            map_offset,
        }))
    }
}

// SAFETY: the backing owns all file descriptors and exposes read-only slices from shared
// mappings. Mappings are initialized once through `OnceLock` and unmapped only in `Drop`, after
// all shared references to the backing are gone.
unsafe impl Send for SharedFdBacking {}

// SAFETY: `plane_data` returns immutable views only, the backing never mutates mapped bytes, and
// `OnceLock` serializes lazy mmap initialization across threads.
unsafe impl Sync for SharedFdBacking {}

impl ExternalBacking for SharedFdBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let plane = self.planes.get(index)?;
        let mapped = self.mapped();
        let range = mapped.ranges.get((*mapped.plane_range.get(index)?)?)?;
        let offset = plane.offset.checked_sub(range.map_offset)?;
        Some(unsafe { std::slice::from_raw_parts(range.ptr.cast::<u8>().add(offset), plane.len) })
    }

    fn backing_bytes(&self) -> Option<usize> {
        match self.kind {
            SharedFdBackingKind::Memfd(_) => self.planes.first().map(|plane| plane.len),
            SharedFdBackingKind::Dmabuf(_) => Some(self.planes.iter().map(|plane| plane.len).sum()),
        }
    }

    fn backing_kind(&self) -> &'static str {
        match self.kind {
            SharedFdBackingKind::Memfd(_) => "memfd",
            SharedFdBackingKind::Dmabuf(_) => "dmabuf",
        }
    }

    fn can_export(&self) -> bool {
        true
    }

    fn residency(&self) -> FrameResidency {
        match self.kind {
            SharedFdBackingKind::Memfd(_) => FrameResidency::HostExternal,
            SharedFdBackingKind::Dmabuf(_) => FrameResidency::Dmabuf,
        }
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        match &self.kind {
            SharedFdBackingKind::Memfd(fd) => Ok(Some(FrameBackingExport::Memfd {
                fd: dup_owned_fd(fd)?,
                len: self
                    .planes
                    .first()
                    .map(|plane| plane.len)
                    .unwrap_or_default(),
            })),
            SharedFdBackingKind::Dmabuf(fds) => {
                let mut planes = Vec::with_capacity(self.planes.len());
                for plane in &self.planes {
                    let fd = fds
                        .get(plane.fd_index)
                        .ok_or(FrameExportError::InvalidDescriptor)?;
                    planes.push(FrameFdPlane {
                        fd: dup_owned_fd(fd)?,
                        offset: plane.offset,
                        len: plane.len,
                    });
                }
                Ok(Some(FrameBackingExport::DmabufPlanes { planes }))
            }
        }
    }
}

impl Drop for SharedFdBacking {
    fn drop(&mut self) {
        if let Some(mapped) = self.mapped.take() {
            self.sync_dmabufs(&mapped.range_fds, false);
            for range in mapped.ranges {
                unsafe {
                    libc::munmap(range.ptr, range.map_len);
                }
            }
        }
    }
}

impl fmt::Debug for SharedFdBacking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedFdBacking")
            .field("kind", &self.backing_kind())
            .field("planes", &self.planes.len())
            .finish()
    }
}

fn dup_owned_fd(fd: &OwnedFd) -> Result<OwnedFd, FrameExportError> {
    let duplicated = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicated < 0 {
        return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

#[cfg(target_os = "linux")]
pub(super) fn create_memfd(name: &str) -> Result<OwnedFd, FrameExportError> {
    let name = std::ffi::CString::new(name).map_err(|err| {
        FrameExportError::Fd(std::io::Error::new(std::io::ErrorKind::InvalidInput, err))
    })?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The size of the memory behind `fd`: a memfd's (or file's) length, a dma-buf's size; `None`
/// when it cannot be told.
pub(super) fn fd_size(fd: &OwnedFd) -> Option<u64> {
    if let Some(size) = regular_file_size(fd) {
        return Some(size);
    }
    // Dma-bufs report their size through `lseek(SEEK_END)`; the offset means nothing to them.
    // SAFETY: plain syscalls on a valid descriptor.
    let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    // SAFETY: as above.
    unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) };
    u64::try_from(end).ok().filter(|&n| n > 0)
}

/// The size of the regular file (memfd) behind `fd`; `None` for anything else.
fn regular_file_size(fd: &OwnedFd) -> Option<u64> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` writes a `stat` for a valid descriptor; it is read only on success.
    let r = unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) };
    if r != 0 {
        return None;
    }
    // SAFETY: `fstat` succeeded.
    let st = unsafe { st.assume_init() };
    ((st.st_mode & libc::S_IFMT) == libc::S_IFREG).then_some(st.st_size as u64)
}

fn system_page_size() -> usize {
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 { ps as usize } else { 4096 }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::super::*;
    use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};

    #[test]
    fn a_memfd_shorter_than_its_descriptor_is_not_read_past_its_end() {
        // Another process sends a 100-byte memfd described as a 64x64 frame: reading the
        // mapping past the file's end used to kill the reader with SIGBUS.
        let fd = create_memfd("styx-short-memfd").unwrap();
        // SAFETY: `fd` is a valid memfd.
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 100) }, 0);
        let fmt = MediaFormat::new(
            FourCc::GREY,
            Resolution::new(64, 64).unwrap(),
            ColorSpace::Srgb,
        );
        let layout = PlaneLayout {
            offset: 0,
            len: 64 * 64,
            stride: 64,
        };
        let frame = FrameLease::from_memfd(FrameMeta::new(fmt, 0), smallvec::smallvec![layout], fd);
        assert!(frame.planes()[0].data().is_empty());
        assert!(frame.visible_rows(0).is_err());
        assert!(frame.to_visible_vec().is_err());
    }

    fn descriptor(
        fourcc: FourCc,
        w: u32,
        h: u32,
        planes: &[(usize, usize, usize)],
    ) -> FrameLeaseDescriptor {
        FrameLeaseDescriptor {
            width: w,
            height: h,
            fourcc,
            timestamp: 0,
            color: ColorSpace::Srgb,
            planes: planes
                .iter()
                .map(|&(offset, len, stride)| FramePlaneDescriptor {
                    offset,
                    len,
                    stride,
                })
                .collect(),
        }
    }

    fn memfd(len: i64) -> std::os::fd::OwnedFd {
        let fd = create_memfd("styx-import").unwrap();
        // SAFETY: `fd` is a valid memfd.
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), len) }, 0);
        fd
    }

    #[test]
    fn imports_refuse_descriptors_their_memory_does_not_hold() {
        let nv12 = descriptor(FourCc::NV12, 4, 2, &[(0, 8, 4), (8, 4, 4)]);
        assert!(FrameLease::from_memfd_import(nv12.clone(), memfd(12)).is_ok());
        // Planes past the memfd's end.
        assert!(FrameLease::from_memfd_import(nv12.clone(), memfd(11)).is_err());
        // Found by the `core_frame_layout` fuzz target: a 57054x57054 I420 frame in an empty
        // memfd (copies of it then allocated gigabytes).
        let huge = descriptor(FourCc::I420, 57054, 57054, &[(16059518511786942174, 0, 0)]);
        assert!(FrameLease::from_memfd_import(huge, memfd(0)).is_err());
        // Planes too short for the frame's rows.
        let short = descriptor(FourCc::NV12, 64, 64, &[(0, 8, 4), (8, 4, 4)]);
        assert!(FrameLease::from_memfd_import(short, memfd(12)).is_err());
        // Dma-buf planes outside their buffers.
        let plane = |offset, len| FrameFdPlane {
            fd: memfd(8),
            offset,
            len,
        };
        let grey = descriptor(FourCc::GREY, 4, 2, &[(0, 8, 4)]);
        assert!(FrameLease::from_dmabuf_import(grey.clone(), vec![plane(0, 8)]).is_ok());
        assert!(FrameLease::from_dmabuf_import(grey.clone(), vec![plane(4, 8)]).is_err());
        assert!(FrameLease::from_dmabuf_import(grey, vec![plane(usize::MAX, 8)]).is_err());
    }

    #[test]
    fn materializing_copies_only_the_bytes_that_are_there() {
        // A layout outside the mapping: its plane reads as empty, and the copy used to panic
        // on the length mismatch or allocate the layout's offset.
        let fmt = MediaFormat::new(
            FourCc::GREY,
            Resolution::new(4, 2).unwrap(),
            ColorSpace::Srgb,
        );
        let layout = PlaneLayout {
            offset: usize::MAX / 2,
            len: 8,
            stride: 4,
        };
        let frame = FrameLease::from_memfd(
            FrameMeta::new(fmt, 0),
            smallvec::smallvec![layout],
            memfd(8),
        );
        let owned = frame.materialize_owned();
        assert_eq!(owned.layouts()[0].len, 0);
    }
}
