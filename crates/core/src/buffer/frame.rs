use smallvec::{SmallVec, smallvec};
use std::{num::NonZeroU32, sync::Arc};

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};

use super::meta::{FrameMeta, FrameMutability, FrameResidency};
#[cfg(target_os = "linux")]
use super::pool::SharedBufferLease;
use super::pool::{BufferLease, BufferPool};

#[cfg(unix)]
mod shared_fd;
#[cfg(unix)]
use shared_fd::SharedFdBacking;
#[cfg(target_os = "linux")]
use shared_fd::create_memfd;

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

pub struct FrameLeaseParts {
    pub meta: FrameMeta,
    pub layouts: SmallVec<[PlaneLayout; 3]>,
    pub buffers: SmallVec<[Vec<u8>; 3]>,
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

    pub fn into_parts(self) -> FrameLeaseParts {
        let layouts = self.layouts.clone();
        if self.external.is_some() {
            FrameLeaseParts {
                meta: self.meta,
                layouts,
                buffers: SmallVec::new(),
            }
        } else {
            let buffers = self.buffers.into_iter().map(|lease| lease.take()).collect();
            FrameLeaseParts {
                meta: self.meta,
                layouts,
                buffers,
            }
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
    if code.is_compressed() {
        FrameResidency::CompressedPacket
    } else {
        FrameResidency::HostOwned
    }
}
