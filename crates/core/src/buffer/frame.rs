use smallvec::{SmallVec, smallvec};
use std::{fmt, num::NonZeroU32, sync::Arc};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::meta::{FrameMeta, FrameMutability, FrameResidency};
#[cfg(target_os = "linux")]
use super::pool::SharedBufferLease;
use super::pool::{BufferLease, BufferPool};

/// External backing for frames when zero-copy sharing external memory.
pub trait ExternalBacking: Send + Sync {
    fn plane_data(&self, index: usize) -> Option<&[u8]>;

    fn backing_bytes(&self) -> Option<usize> {
        None
    }

    fn backing_kind(&self) -> &'static str {
        "external"
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::HostExternal
    }

    #[cfg(unix)]
    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
}

#[derive(Debug)]
pub struct PlaneMut<'a> {
    data: &'a mut [u8],
    stride: usize,
}

impl<'a> Plane<'a> {
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
}

impl<'a> PlaneMut<'a> {
    pub fn data(&mut self) -> &mut [u8] {
        self.data
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct PlaneLayout {
    pub offset: usize,
    pub len: usize,
    pub stride: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct FramePlaneDescriptor {
    pub offset: usize,
    pub len: usize,
    pub stride: usize,
}

impl From<PlaneLayout> for FramePlaneDescriptor {
    fn from(layout: PlaneLayout) -> Self {
        Self {
            offset: layout.offset,
            len: layout.len,
            stride: layout.stride,
        }
    }
}

impl From<FramePlaneDescriptor> for PlaneLayout {
    fn from(plane: FramePlaneDescriptor) -> Self {
        Self {
            offset: plane.offset,
            len: plane.len,
            stride: plane.stride,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct FrameLeaseDescriptor {
    pub width: u32,
    pub height: u32,
    pub fourcc: crate::format::FourCc,
    pub timestamp: u64,
    pub color: crate::format::ColorSpace,
    pub planes: Vec<FramePlaneDescriptor>,
}

impl FrameLeaseDescriptor {
    pub fn to_meta(&self) -> Option<FrameMeta> {
        let resolution = crate::format::Resolution::new(self.width, self.height)?;
        let format = crate::format::MediaFormat::new(self.fourcc, resolution, self.color);
        Some(FrameMeta::new(format, self.timestamp))
    }

    pub fn layouts(&self) -> SmallVec<[PlaneLayout; 3]> {
        self.planes.iter().copied().map(PlaneLayout::from).collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameExportError {
    #[error("frame backing is process-local and cannot be exported without copying")]
    NotExportable,
    #[error("fd operation failed: {0}")]
    Fd(std::io::Error),
    #[error("fd mmap failed: {0}")]
    Mmap(std::io::Error),
    #[error("frame descriptor is invalid")]
    InvalidDescriptor,
    #[error("plane count mismatch: descriptor has {expected}, backing has {actual}")]
    PlaneCountMismatch { expected: usize, actual: usize },
}

#[cfg(unix)]
#[derive(Debug)]
pub struct FrameFdPlane {
    pub fd: OwnedFd,
    pub offset: usize,
    pub len: usize,
}

#[cfg(unix)]
#[derive(Debug)]
pub enum FrameBackingExport {
    Memfd { fd: OwnedFd, len: usize },
    DmabufPlanes { planes: Vec<FrameFdPlane> },
}

pub struct FrameLease {
    meta: FrameMeta,
    buffers: SmallVec<[BufferLease; 3]>,
    layouts: SmallVec<[PlaneLayout; 3]>,
    external: Option<Arc<dyn ExternalBacking>>,
}

impl FrameLease {
    pub fn single_plane(
        mut meta: FrameMeta,
        mut buffer: BufferLease,
        len: usize,
        stride: usize,
    ) -> Self {
        buffer.resize(len);
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            layouts: smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffers: smallvec![buffer],
            external: None,
        }
    }

    /// # Safety
    /// The caller must write every byte of the buffer before the frame is read.
    pub unsafe fn single_plane_uninit(
        mut meta: FrameMeta,
        mut buffer: BufferLease,
        len: usize,
        stride: usize,
    ) -> Self {
        unsafe { buffer.resize_uninit(len) };
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            layouts: smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffers: smallvec![buffer],
            external: None,
        }
    }

    pub fn multi_plane(
        mut meta: FrameMeta,
        buffers: SmallVec<[BufferLease; 3]>,
        layouts: SmallVec<[PlaneLayout; 3]>,
    ) -> Self {
        debug_assert_eq!(buffers.len(), layouts.len());
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            buffers,
            layouts,
            external: None,
        }
    }

    pub fn from_external(
        mut meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        backing: Arc<dyn ExternalBacking>,
    ) -> Self {
        if meta.residency.is_none() {
            meta.residency = Some(backing.residency());
        }
        meta.mutability = FrameMutability::ReadOnly;
        Self {
            meta,
            buffers: SmallVec::new(),
            layouts,
            external: Some(backing),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn single_plane_shared(
        mut meta: FrameMeta,
        mut buffer: SharedBufferLease,
        len: usize,
        stride: usize,
    ) -> Result<Self, FrameExportError> {
        buffer.try_resize(len)?;
        if meta.residency.is_none() {
            meta.residency = Some(FrameResidency::HostExternal);
        }
        Ok(Self::from_external(
            meta,
            smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffer.into_external_backing(1),
        ))
    }

    #[cfg(target_os = "linux")]
    pub fn multi_plane_shared(
        mut meta: FrameMeta,
        mut buffer: SharedBufferLease,
        layouts: SmallVec<[PlaneLayout; 3]>,
    ) -> Result<Self, FrameExportError> {
        let len = layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(0);
        buffer.try_resize(len)?;
        if meta.residency.is_none() {
            meta.residency = Some(FrameResidency::HostExternal);
        }
        let plane_count = layouts.len();
        Ok(Self::from_external(
            meta,
            layouts,
            buffer.into_external_backing(plane_count),
        ))
    }

    #[cfg(unix)]
    pub fn from_shared_fd(
        meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        fd: OwnedFd,
    ) -> Self {
        Self::from_memfd(meta, layouts, fd)
    }

    #[cfg(unix)]
    pub fn from_memfd(
        mut meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        fd: OwnedFd,
    ) -> Self {
        meta.residency = Some(FrameResidency::HostExternal);
        let len = layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(0);
        let backing = SharedFdBacking::memfd(fd, len, layouts.len());
        Self::from_external(meta, layouts, Arc::new(backing))
    }

    #[cfg(unix)]
    pub fn from_dmabuf(
        mut meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        planes: Vec<FrameFdPlane>,
    ) -> Result<Self, FrameExportError> {
        if planes.len() != layouts.len() {
            return Err(FrameExportError::PlaneCountMismatch {
                expected: layouts.len(),
                actual: planes.len(),
            });
        }
        meta.residency = Some(FrameResidency::Dmabuf);
        let backing = SharedFdBacking::dmabuf(planes);
        Ok(Self::from_external(meta, layouts, Arc::new(backing)))
    }

    #[cfg(unix)]
    pub fn from_memfd_import(
        descriptor: FrameLeaseDescriptor,
        fd: OwnedFd,
    ) -> Result<Self, FrameExportError> {
        let meta = descriptor
            .to_meta()
            .ok_or(FrameExportError::InvalidDescriptor)?;
        Ok(Self::from_memfd(meta, descriptor.layouts(), fd))
    }

    #[cfg(unix)]
    pub fn from_dmabuf_import(
        descriptor: FrameLeaseDescriptor,
        planes: Vec<FrameFdPlane>,
    ) -> Result<Self, FrameExportError> {
        let meta = descriptor
            .to_meta()
            .ok_or(FrameExportError::InvalidDescriptor)?;
        Self::from_dmabuf(meta, descriptor.layouts(), planes)
    }

    pub fn meta(&self) -> &FrameMeta {
        &self.meta
    }

    pub fn meta_mut(&mut self) -> &mut FrameMeta {
        &mut self.meta
    }

    pub fn is_external(&self) -> bool {
        self.external.is_some()
    }

    pub fn residency(&self) -> FrameResidency {
        self.meta
            .residency
            .unwrap_or_else(|| match self.external.as_ref() {
                Some(backing) => backing.residency(),
                None => default_owned_residency(self.meta.format.code),
            })
    }

    pub fn mutability(&self) -> FrameMutability {
        self.meta.mutability
    }

    pub fn payload_bytes(&self) -> usize {
        self.layouts.iter().map(|layout| layout.len).sum()
    }

    pub fn descriptor(&self) -> FrameLeaseDescriptor {
        let format = self.meta.format;
        FrameLeaseDescriptor {
            width: format.resolution.width.get(),
            height: format.resolution.height.get(),
            fourcc: format.code,
            timestamp: self.meta.timestamp,
            color: format.color,
            planes: self.layouts.iter().copied().map(Into::into).collect(),
        }
    }

    pub fn external_backing_bytes(&self) -> Option<usize> {
        self.external.as_ref().map(|backing| {
            backing
                .backing_bytes()
                .unwrap_or_else(|| self.payload_bytes())
        })
    }

    pub fn external_backing_kind(&self) -> Option<&'static str> {
        self.external.as_ref().map(|backing| backing.backing_kind())
    }

    pub fn planes(&self) -> SmallVec<[Plane<'_>; 3]> {
        if let Some(backing) = &self.external {
            self.layouts
                .iter()
                .enumerate()
                .map(|(idx, layout)| {
                    let slice = backing
                        .plane_data(idx)
                        .map(|s| {
                            let end = layout.offset.saturating_add(layout.len);
                            s.get(layout.offset..end).unwrap_or(&[])
                        })
                        .unwrap_or(&[]);
                    Plane {
                        data: slice,
                        stride: layout.stride,
                    }
                })
                .collect()
        } else {
            self.layouts
                .iter()
                .zip(self.buffers.iter())
                .map(|(layout, buf)| {
                    let slice = buf
                        .as_slice()
                        .get(layout.offset..layout.offset + layout.len)
                        .unwrap_or(&[]);
                    Plane {
                        data: slice,
                        stride: layout.stride,
                    }
                })
                .collect()
        }
    }

    pub fn planes_mut(&mut self) -> SmallVec<[PlaneMut<'_>; 3]> {
        if self.external.is_some() {
            return self
                .layouts
                .iter()
                .map(|layout| PlaneMut {
                    data: &mut [],
                    stride: layout.stride,
                })
                .collect();
        }
        self.layouts
            .iter()
            .zip(self.buffers.iter_mut())
            .map(|(layout, buf)| {
                let len = layout.offset + layout.len;
                if buf.len() < len {
                    buf.resize(len);
                }
                let slice = buf
                    .as_mut_slice()
                    .get_mut(layout.offset..layout.offset + layout.len)
                    .unwrap_or(&mut []);
                PlaneMut {
                    data: slice,
                    stride: layout.stride,
                }
            })
            .collect()
    }

    pub fn layouts(&self) -> SmallVec<[PlaneLayout; 3]> {
        self.layouts.clone()
    }

    pub fn layout_slice(&self) -> &[PlaneLayout] {
        &self.layouts
    }

    #[cfg(unix)]
    pub fn export_backing(&self) -> Result<FrameBackingExport, FrameExportError> {
        self.external
            .as_ref()
            .ok_or(FrameExportError::NotExportable)?
            .export_backing()?
            .ok_or(FrameExportError::NotExportable)
    }

    #[cfg(unix)]
    pub fn export_descriptor_and_backing(
        &self,
    ) -> Result<(FrameLeaseDescriptor, FrameBackingExport), FrameExportError> {
        Ok((self.descriptor(), self.export_backing()?))
    }

    #[cfg(target_os = "linux")]
    pub fn export_or_copy_memfd(
        &self,
    ) -> Result<(FrameLeaseDescriptor, FrameBackingExport), FrameExportError> {
        if let Some(backing) = self.external.as_ref()
            && let Some(export) = backing.export_backing()?
        {
            return Ok((self.descriptor(), export));
        }
        Ok((
            self.descriptor(),
            FrameBackingExport::Memfd {
                fd: self.copy_to_memfd()?,
                len: self.backing_span_len(),
            },
        ))
    }

    pub fn plane_strides(&self) -> SmallVec<[usize; 3]> {
        self.layouts.iter().map(|l| l.stride).collect()
    }

    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        FrameMeta,
        SmallVec<[PlaneLayout; 3]>,
        SmallVec<[Vec<u8>; 3]>,
    ) {
        let layouts = self.layouts.clone();
        if self.external.is_some() {
            (self.meta, layouts, SmallVec::new())
        } else {
            let buffers = self.buffers.into_iter().map(|lease| lease.take()).collect();
            (self.meta, layouts, buffers)
        }
    }

    pub fn materialize_owned(&self) -> Self {
        let max_len = self
            .layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(1)
            .max(1);
        let pool = BufferPool::with_limits(self.layouts.len().max(1), max_len, self.layouts.len());
        let buffers = self
            .planes()
            .into_iter()
            .zip(self.layouts.iter().copied())
            .map(|(plane, layout)| {
                let required = layout.offset.saturating_add(layout.len);
                let mut lease = pool.lease();
                lease.resize(required);
                if layout.len > 0 {
                    let dst = &mut lease.as_mut_slice()[layout.offset..required];
                    dst.copy_from_slice(plane.data());
                }
                lease
            })
            .collect();
        let mut meta = self.meta.clone();
        meta.residency = Some(FrameResidency::HostOwned);
        meta.mutability = FrameMutability::Mutable;
        FrameLease::multi_plane(meta, buffers, self.layouts.clone())
    }

    fn backing_span_len(&self) -> usize {
        self.layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(0)
    }

    #[cfg(target_os = "linux")]
    fn copy_to_memfd(&self) -> Result<OwnedFd, FrameExportError> {
        let fd = create_memfd("styx-frame")?;
        let len = self.backing_span_len();
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
        }
        for (plane, layout) in self.planes().into_iter().zip(self.layouts.iter()) {
            let data = plane.data();
            let copy_len = data.len().min(layout.len);
            let mut written = 0usize;
            while written < copy_len {
                let ret = unsafe {
                    libc::pwrite(
                        fd.as_raw_fd(),
                        data[written..copy_len].as_ptr().cast(),
                        copy_len - written,
                        layout.offset.saturating_add(written) as libc::off_t,
                    )
                };
                if ret < 0 {
                    return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
                }
                if ret == 0 {
                    return Err(FrameExportError::Fd(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "short memfd write",
                    )));
                }
                written = written.saturating_add(ret as usize);
            }
        }
        Ok(fd)
    }
}

#[cfg(unix)]
struct SharedFdBacking {
    kind: SharedFdBackingKind,
    planes: Vec<SharedFdPlane>,
    mapped: std::sync::OnceLock<Vec<Option<MappedFdRange>>>,
}

#[cfg(unix)]
enum SharedFdBackingKind {
    Memfd(OwnedFd),
    Dmabuf(Vec<OwnedFd>),
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct SharedFdPlane {
    fd_index: usize,
    offset: usize,
    len: usize,
}

#[cfg(unix)]
struct MappedFdRange {
    ptr: *mut core::ffi::c_void,
    map_len: usize,
    map_offset: usize,
}

#[cfg(unix)]
impl SharedFdBacking {
    fn memfd(fd: OwnedFd, len: usize, plane_count: usize) -> Self {
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

    fn dmabuf(planes: Vec<FrameFdPlane>) -> Self {
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

#[cfg(unix)]
unsafe impl Send for SharedFdBacking {}

#[cfg(unix)]
unsafe impl Sync for SharedFdBacking {}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
impl fmt::Debug for SharedFdBacking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedFdBacking")
            .field("kind", &self.backing_kind())
            .field("planes", &self.planes.len())
            .finish()
    }
}

#[cfg(unix)]
fn dup_owned_fd(fd: &OwnedFd) -> Result<OwnedFd, FrameExportError> {
    let duplicated = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicated < 0 {
        return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

#[cfg(target_os = "linux")]
fn create_memfd(name: &str) -> Result<OwnedFd, FrameExportError> {
    let name = std::ffi::CString::new(name).map_err(|err| {
        FrameExportError::Fd(std::io::Error::new(std::io::ErrorKind::InvalidInput, err))
    })?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn system_page_size() -> usize {
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 { ps as usize } else { 4096 }
}

pub fn plane_layout_from_dims(
    width: NonZeroU32,
    height: NonZeroU32,
    bytes_per_pixel: usize,
) -> PlaneLayout {
    let stride = width.get() as usize * bytes_per_pixel;
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

pub fn plane_layout_with_stride(
    _width: NonZeroU32,
    height: NonZeroU32,
    stride: usize,
) -> PlaneLayout {
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

fn default_owned_residency(code: crate::format::FourCc) -> FrameResidency {
    if matches!(
        code,
        c if c == crate::format::FourCc::new(*b"MJPG")
            || c == crate::format::FourCc::new(*b"JPEG")
            || c == crate::format::FourCc::new(*b"H264")
            || c == crate::format::FourCc::new(*b"H265")
            || c == crate::format::FourCc::new(*b"HEVC")
    ) {
        FrameResidency::CompressedPacket
    } else {
        FrameResidency::HostOwned
    }
}
