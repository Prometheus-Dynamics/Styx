use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::{ExternalBacking, FrameBackingExport, FrameExportError, FrameFdPlane, FrameResidency};

pub(super) struct SharedFdBacking {
    kind: SharedFdBackingKind,
    planes: Vec<SharedFdPlane>,
    mapped: std::sync::OnceLock<Vec<Option<MappedFdRange>>>,
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

    fn mapped_ranges(&self) -> &Vec<Option<MappedFdRange>> {
        self.mapped.get_or_init(|| {
            self.planes
                .iter()
                .map(|plane| self.map_plane(*plane).ok().flatten())
                .collect()
        })
    }

    fn map_plane(&self, plane: SharedFdPlane) -> Result<Option<MappedFdRange>, FrameExportError> {
        if plane.len == 0 {
            return Ok(None);
        }
        let Some(fd) = self.fd(plane.fd_index) else {
            return Ok(None);
        };
        let page_size = system_page_size();
        let map_offset = plane.offset - (plane.offset % page_size);
        let delta = plane.offset - map_offset;
        let map_len = delta.saturating_add(plane.len);
        let addr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
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
        let range = self.mapped_ranges().get(index)?.as_ref()?;
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
            for range in mapped.into_iter().flatten() {
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

fn system_page_size() -> usize {
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 { ps as usize } else { 4096 }
}
